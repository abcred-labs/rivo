### rivo-language-schema: 1
### locale: en-US
### display-name: English (United States)
### revision: 2026-09-11.1
### min-rivo-version: 0.1.0

common-ok = OK
common-cancel = Cancel
common-back = Back
common-reload = Reload
language-current = Current language: { $locale }
language-reloaded = Language reloaded: { $locale }
error-language-invalid = Unsupported language: { $locale }
error-language-file-invalid = Invalid language file: { $reason }
error-language-message-missing = Missing message: { $message }
error-language-reload-failed = Could not reload language: { $reason }
phase-idle = Idle
phase-running = Running
phase-waiting = Waiting for confirmation
phase-cancelling = Cancelling
phase-finished = Complete
phase-error = Error
settings-title = Session Settings
approval-title = Approval required
help-title = Help
startup-title = Rivo · Startup Configuration
startup-tagline = Let the agent work in your terminal · Choose settings, then press Enter to start
startup-selection-help = Vendor → Model → Auto / explicit Provider; Enter/Space switch, ←/→ choose previous/next.
startup-capability = Capability mode
startup-policy = Safety policy
startup-vendor = Vendor
startup-model = Model
startup-provider-auto = Auto
startup-provider-explicit = Explicit { $provider }
startup-unselected = Unselected
startup-model-default = Automatic (catalog default)
startup-provider = Provider
startup-network = Network
startup-browser = Browser
startup-memory = Memory recall
startup-trace = Trace
startup-mcp = MCP
startup-start = Start
startup-route-preview = Route preview: { $vendor } / { $model } → { $provider } ({ $protocol }, priority={ $priority })
startup-route-unresolved = Route preview: unresolved; cannot start
startup-notice = Notice
sidebar-title = ● Status
drawer-title = ● Activity / Errors · Esc Back
input-placeholder = › Enter a task or /help
input-title = › Input · Enter Submit · Ctrl+J New Line
command-palette-title = ✻ Command Palette
settings-scope = Changes affect subsequent turns in this session only.
settings-status = Status
settings-hint = Notice
workspace-empty = No registered workspaces
help-keyhint = ↑/↓ Scroll · Esc Close Help
palette-keyhint = ↑/↓ Select · Tab Complete · Enter Run · Esc Close
settings-keyhint = Arrow/Tab Browse · Enter Edit · Space Toggle · Ctrl+Enter Apply · Esc Cancel
workspace-keyhint = ↑/↓ Select · Enter Open · Esc Close
confirm-keyhint = y Accept · n Deny · Enter Confirm · Esc Close
interaction-keyhint = ↑/↓ Select · 1-9 Quick Select · Enter Submit
diagnostic-keyhint = r Retry · Enter/Esc Back
default-keyhint = / Commands · F6 Settings · Tab Focus · ↑/↓ Scroll · ? Help · Esc Clear
notice-plan-approved = Plan approved
notice-plan-rejected = Plan rejected
notice-plan-started = Plan execution started
notice-step-started = Plan step started
notice-step-completed = Plan step completed
notice-step-blocked = Plan step blocked
notice-plan-completed = Plan completed
notice-goal-completed = Goal completed
notice-goal-blocked = Goal blocked
notice-tool-completed = Tool completed
notice-media-ready = Media ready
notice-media-failed = Media operation failed
notice-context-compressed = Context compressed; omitted messages
notice-subagent-queued = Subagent queued
notice-subagent-completed = Subagent completed
notice-confirmation-required = Confirmation required
notice-interaction-required = Interaction required
notice-memory-saved = Memory saved
notice-memory-rejected = Memory candidate rejected
notice-memory-deleted = Memory deleted
notice-retry-available = Retry available
help-body = /exit /cancel /retry /sessions /new /errors /permissions /language /settings /provider /model /network /browser /mcp /a2a /subagents /trace /memory
language-switched = Language switched
error-language-runtime-missing = Language runtime is unavailable
error-language-switch-failed = Could not switch language
notice-plan-updated = Plan updated
notice-goal-updated = Goal updated
notice-goal-continuation = Goal continuation scheduled
notice-goal-audit = Goal audit
goal-audit-complete = satisfied
goal-audit-continue = continue
notice-tool-requested = Tool requested
notice-media-started = Media operation in progress:
notice-subagent-confirmation = Subagent confirmation requested
notice-subagent-started = Subagent started
notice-model-selected = Model selected
notice-prompt-prepared = Prompt prepared
notice-session-settings-applied = Session settings applied; the next turn will use them.
tool-summary-awaiting-policy = Awaiting policy evaluation
tool-summary-needs-confirmation = Requires user confirmation
notice-memory-candidate = Memory candidate
notice-memory-recalled = Memories recalled
notice-memory-injected = Memories injected
memory-candidate-actions = press y to save or n to reject
diagnostic-recovery = Review diagnostics, then retry or start a new session
error-workspace-switch-blocked = Finish the pending interaction before switching workspace
error-settings-blocked = Finish the pending interaction before opening session settings
error-route-unresolved = The current session has no resolved model route
confirmation-no-side-effects = No additional side effects
confirmation-actions-session = y allow once · a allow for this session · n/Esc deny
confirmation-actions-once = y allow once · n/Esc deny
memory-candidate-title = Memory candidate
label-source-agent = Source agent
label-task = task
label-tool = Tool
label-summary = Summary
label-side-effects = Side effects
label-candidate = Candidate
label-summary-hash = Summary hash
error-terminal-too-small = Terminal is too small:
label-minimum = minimum
terminal-resize-hint = Resize the terminal to continue.
label-errors = errors
settings-mode-browse = Browse · Enter edit · Space toggle
settings-mode-edit-field = Edit field · Arrow preview · Enter confirm · Esc cancel
settings-mode-edit-text = Edit text · Arrow move cursor · Enter confirm · Esc cancel
settings-mode-multiselect = Multi-select · Space toggle · Enter confirm · Esc cancel
workspace-title = ✻ Workspace
diagnostic-title = Error detail
label-stage = Stage
label-error-code = Error code
label-retryable = Retryable
label-recovery = Recovery
label-message = Message
label-full-cause = Full cause
label-freeform = Free-form input
interaction-detail-keyhint = ↑/↓ select · 1-9 quick select · Enter submit · Esc requires an explicit response
interaction-title = ✻ Interaction · response required
label-budget = Budget
label-steps = steps
label-none = None
label-targets = Targets
label-program = Program
label-capability = Capability
label-policy = Policy
label-constraints = Constraints
label-read = read
label-write = write
label-network = network
label-timeout = timeout
label-output-limit = output limit
label-resolved-provider = Resolved provider
label-persistent-instructions = persistent instructions
label-truncated =  (truncated)
sidebar-session = ── Session ──
sidebar-user-selection = User selection
label-unselected = unselected
sidebar-resolved-route = Resolved route
sidebar-route-pending = unconfirmed (waiting for session route)
label-model = Model
label-runtime-status = Runtime status
sidebar-policy = ── Policy ──
label-capability-mode = Capability mode
label-safety-policy = Safety policy
sidebar-runtime = ── Runtime ──
label-browser = Browser
label-off = off
label-memory = Memory
sidebar-budget = ── Budget ──
conversation-collapsed = … Collapsed · Enter expand
settings-group-routing = MODEL ROUTING
settings-group-runtime = RUNTIME
settings-group-context = CONTEXT
settings-group-agents = AGENTS
settings-edit-number = Enter a number, then press Enter to confirm
settings-no-options = No options available; create configuration with the corresponding command first.
cli-language-help = Manage local UI language files.

    Usage: rivo language <COMMAND>

    Commands: path, list, show, validate, install, remove, remote-list, download, update, rollback

    Options: -h, --help  Show help
cli-sessions-help = Manage local Workspace session history.

    Usage: rivo sessions [COMMAND]

    Commands: path, backup, reset, migrate, list, show, resume, attach, delete, clear

    Options: -h, --help  Show help
cli-workspace-help = Manage local workspaces.

    Usage: rivo workspace list|add|open|show|rename|rebind|remove

    Options: -h, --help  Show help
cli-diagnostics-help = Create a redacted diagnostics bundle.

    Usage: rivo diagnostics create <PATH> [--yes]

    Options: --yes  Skip confirmation; -h, --help  Show help
cli-instance-help = Inspect local Rivo instances.

    Usage: rivo instance [COMMAND]

    Commands: list, endpoint, skill

    Options: -h, --help  Show help
cli-mcp-help = Manage configured MCP servers.

    Usage: rivo mcp <COMMAND>

    Commands: list, show, inspect, login, install, enable, disable, remove

    Options: -h, --help  Show help
cli-a2a-help = Manage A2A peers, delegation and the local A2A server.

    Usage: rivo a2a <COMMAND>

    Commands: discover, pair, peers, inspect, revoke, delegate, control, cancel, recover, serve, pairings, approvals, provision

    Options: -h, --help  Show help
cli-a2a-discover-help = Discover local A2A candidates.

    Usage: rivo a2a discover [--timeout-seconds <1..=300>]
cli-a2a-pair-help = Pair an A2A peer.

    Usage: rivo a2a pair <LOCAL_IDENTITY_LABEL> --endpoint <HTTPS_URL> --spki-sha256 <SHA256> [--transport <mtls|psk>] [--secret-stdin]
cli-a2a-peers-help = List paired A2A peers.

    Usage: rivo a2a peers
cli-a2a-inspect-help = Show A2A peer details.

    Usage: rivo a2a inspect <PEER_ID>
cli-a2a-revoke-help = Revoke an A2A peer.

    Usage: rivo a2a revoke <PEER_ID> [--yes]
cli-a2a-delegate-help = Delegate a task to an A2A peer; the body is read only from stdin.

    Usage: rivo a2a delegate <PEER_ID> <SKILL_ID> --message-stdin --yes [--stream] [--transport psk --secret-stdin]
cli-a2a-control-help = Call an approved instance-control operation on a peer.

    Usage: rivo a2a control <PEER_ID> <TARGET_INSTANCE_ID> <METHOD> --params-stdin [--yes]
cli-a2a-cancel-help = Cancel a remote A2A task.

    Usage: rivo a2a cancel <PEER_ID> <TASK_ID> --yes [--transport psk --secret-stdin]
cli-a2a-recover-help = Recover a remote A2A task.

    Usage: rivo a2a recover <PEER_ID> <TASK_ID> [--transport psk --secret-stdin]
cli-a2a-serve-help = Start the local A2A server.

    Usage: rivo a2a serve [<SERVER_PROFILE>] --config <OWNER_A2A_SERVER_TOML>
cli-a2a-pairings-help = Manage local A2A pairing requests.

    Usage: rivo a2a pairings list --config <OWNER_A2A_SERVER_TOML>
    rivo a2a pairings <approve|reject> <REQUEST_ID> --config <OWNER_A2A_SERVER_TOML>
cli-a2a-approvals-help = Manage local A2A approvals.

    Usage: rivo a2a approvals list --config <OWNER_A2A_SERVER_TOML>
    rivo a2a approvals <approve|reject> <APPROVAL_ID> --config <OWNER_A2A_SERVER_TOML>
cli-a2a-provision-help = Manage local A2A test identity material.

    Usage: rivo a2a provision --yes --bind <PRIVATE_IP:PORT> --server-identity-ref <REF> --issuer-ref <REF>
    rivo a2a provision <check|cleanup> --server-identity-ref <REF> --issuer-ref <REF> [--yes]
cli-a2a-no-pending-approvals = No A2A approval requests are pending for local decision
cli-a2a-test-identities-cleaned = test A2A identities cleaned
cli-a2a-peer-revoked = revoked {$id}
cli-a2a-peer-grant-revoked = peer grant {$id} revoked
cli-a2a-approval-decided = approval {$id} {$operation}d once
cli-a2a-storage-worker-interrupted = A2A local storage worker was interrupted
cli-a2a-provision-usage = Usage: rivo a2a provision --yes --bind <private-ip:port> --server-identity-ref a2a-test-... --issuer-ref a2a-test-...
cli-a2a-server-identity-missing = --server-identity-ref is required
cli-a2a-issuer-missing = --issuer-ref is required
cli-a2a-provision-confirm-required = provision requires explicit --yes
cli-a2a-test-identity-required = provision only allows a2a-test- identity references
cli-a2a-bind-format-invalid = bind must be IP:port
cli-a2a-bind-address-invalid = bind must be an RFC1918/ULA address; loopback requires RIVO_A2A_LOCAL_TEST=1
cli-a2a-test-identity-exists = test identity already exists; use a new reference to avoid overwriting it
cli-a2a-spki-compute-failed = Cannot compute SPKI from the generated Server certificate
cli-a2a-server-keychain-write-failed = Cannot write Server TLS bundle to the OS keychain
cli-a2a-client-ca-write-failed = Cannot write profile Client CA to the OS keychain
cli-a2a-cleanup-confirm-required = provision cleanup requires explicit --yes
cli-a2a-flag-missing = {$flag} is required
cli-a2a-cleanup-identity-required = cleanup only allows a2a-test- identity references
cli-a2a-check-identity-required = check only allows a2a-test- identity references
cli-a2a-server-keychain-read-failed = Cannot read the Server v2 keychain entry
cli-a2a-client-ca-read-failed = Cannot read the Client CA v2 keychain entry
cli-a2a-spki-parse-failed = Cannot parse the Server certificate SPKI
cli-a2a-inspect-usage = Usage: rivo a2a inspect <peer-id>
cli-a2a-peer-not-found = A2A peer not found
cli-a2a-revoke-usage = Usage: rivo a2a revoke <peer-id> [--yes]
cli-a2a-peer-not-paired = A2A peer is not paired or does not exist
cli-a2a-identity-missing = A2A peer has no local mTLS identity
cli-a2a-task-not-found = No persisted task mapping exists locally
cli-a2a-task-peer-mismatch = A2A task mapping belongs to a different peer
cli-a2a-transport-mismatch = A2A peer transport does not match the requested transport
cli-a2a-top-usage = Usage: rivo a2a <COMMAND>
cli-a2a-discovery-interrupted = A2A discovery task was interrupted
cli-a2a-empty-message = A2A delegation stdin cannot be empty
cli-a2a-control-json-invalid = A2A control stdin must be a JSON object
cli-a2a-control-json-object = A2A control parameters must be a JSON object
cli-a2a-recover-failed = A2A task recovery failed; business delegation was not resent
notice-locale-pending-turn = Apply after the current turn or retry finishes
notice-settings-applying = Applying Session settings
notice-cancel-in-progress = Cancelling the current Turn; press q again to exit
notice-history-delete-cancelled = History deletion confirmation cancelled
error-no-retry-step = The current error has no retryable step
error-history-expired-resume = The selected history session has expired and cannot be resumed; create a new session
error-history-expired-delete = The selected history session has expired and cannot be deleted; use the CLI to inspect retention
notice-session-switched = Switched to session {$id}
notice-diagnostics-cleared = Local diagnostics cleared
error-diagnostics-clear = Failed to clear diagnostics: {$error}
notice-no-persistent-grants = This session has no persistent grants
notice-persistent-history-disabled = Persistent history is not enabled for this session
notice-turn-cancel-first = Finish or cancel the current Turn before continuing
notice-workspace-empty = No Workspace is available
error-workspace-read = Failed to read Workspace: {$error}
notice-session-not-ready = Session is not ready
notice-model-required = Select a model first
error-model-invalid = The selected model is not in the valid catalog
notice-no-retry = No retryable step
notice-mcp-disabled = MCP is disabled for the current Turn
notice-a2a-cleared = A2A peers cleared for the current Turn
notice-memory-next-turn = Memory candidates are confirmed by events; recall applies to the next Turn only
error-subagents-positive = Maximum subagents must be a positive integer
notice-unknown-command-help = Unknown command; use /help for help
notice-interaction-select-or-input = Select an option or enter a response first
notice-interaction-response-required = Interaction still requires an explicit response
notice-session-approval-unsupported = This confirmation does not support session-wide approval; use y or n
notice-routing-draft-incompatible = Model changes made the Provider or reasoning incompatible; only the local draft was updated
notice-language-catalog-status = Current UI catalog={$current}; available: {$available}
notice-tui-cancel-exit = Cancelling the current Turn; wait for the terminal state, then run /exit again
notice-error-filter = Error filter: {$filter}
notice-persistent-grants = Current session grants: {$grants}
notice-revoke-grants = Revoked {$count} session grant(s)
notice-session-delete-confirm = Permanently delete history session {$id}. Run /sessions delete {$id} again to confirm; Esc or another delete command cancels.
notice-session-deleted = Deleted history session {$id}
error-session-delete = Failed to delete history session: {$error}
notice-history-help = History: ↑/↓ select, Enter resume, Delete delete (confirm twice), /new create a session
notice-provider-settings-open = Current provider={$provider}; Session Settings opened
error-route-not-ready = Session is not ready to validate the route
notice-provider-switching = Switching Provider: {$provider}
notice-model-settings-open = Current model={$model}; Session Settings opened; available: {$models}
notice-model-switching = Switching model: {$model}
notice-network-state = Current Turn network={$enabled}
notice-browser-state = Current Turn browser={$enabled}; page content is not persisted in session history
notice-browser-status = Current Turn browser={$enabled}; the browser must be started by the user with a dedicated CDP profile
notice-mcp-server-list = MCP server is not configured or enabled: {$server}; available: {$available}
notice-mcp-server-selected = Current Turn MCP server={$server}
notice-mcp-server-disabled = MCP server is not configured or enabled: {$server}; available: {$available}
error-a2a-selection = A2A peer cannot be selected: {$error}
notice-a2a-selected = Current Turn selected A2A peer={$peer}
notice-memory-delete = Requested memory deletion: {$id}
notice-subagents-state = Current Turn subagents={$enabled}
notice-subagents-max = Current Turn max_subagents={$max}
notice-subagents-status = Current Turn subagents={$enabled}, max_subagents={$max}
notice-trace-state = Current Turn trace={$enabled}
error-session-stream-ended = Session event stream ended unexpectedly
notice-memory-state = Current Turn memory_recall={$enabled}

    Usage: rivo a2a <COMMAND>

    Commands: discover, pair, peers, inspect, revoke, delegate, control, cancel, recover, serve, pairings, approvals, provision

    Options: -h, --help  Show help
cli-config-help = Manage Rivo Vendor, Model, Provider and Credential.

    Usage: rivo config [COMMAND]

    Commands: path, show, validate, set, unset, credentials, route, vendor, model, provider

    Options: -h, --help  Show help
cli-run-help = Run a Rivo task.

    Usage: rivo run --prompt <TEXT> [OPTIONS]
           rivo run --stdin [OPTIONS]

    Options: --language <BCP47> --vendor <ID> --model <ID> --provider-id <ID> --format <text|jsonl> --approval-mode <prompt|deny|manifest>

    Tasks must be supplied with --prompt or --stdin.
cli-root-help = Rivo — General Agent CLI

    Usage:
      rivo <COMMAND>
      rivo run --prompt <TEXT> [OPTIONS]
      rivo run --stdin [OPTIONS]

    Commands:
      run          Run a task
      tui          Open the interactive terminal workspace
      config       Manage Vendor, Model, Provider and Credential
      language     Manage local UI language files
      sessions     View, resume or delete local sessions
      mcp          Manage configured MCP servers
      a2a          Manage A2A peers and the local A2A server
      instance     Inspect the local Rivo instance
      diagnostics  Create a redacted diagnostics bundle
      upgrade      Check platform updates (manual archive replacement)
      app-server   Start the local JSON-RPC bridge

    Options:
      -h, --help     Show help
      -V, --version  Show version

    Common task options: --vendor, --model, --provider-id, --auto-provider, --format and --approval-mode.
    Tasks must be supplied explicitly with --prompt or --stdin. Run `rivo <COMMAND> --help` for command help.
cli-tui-help = Open the interactive terminal workspace.

    Usage: rivo tui [OPTIONS]

    Options:
          --workspace <PATH>  Workspace for this TUI session
      -h, --help              Show help

    Examples:
      rivo tui
      rivo tui --workspace C:\\path\\to\\project

cli-tui-terminal-required = rivo tui requires an interactive terminal; run it locally, or use `rivo run --format jsonl` for automation
cli-tui-current-directory-missing = Could not determine the current working directory
cli-tui-startup-cancelled = TUI startup cancelled
cli-tui-startup-save-failed = Could not save startup configuration: {$error}
cli-tui-first-launch-terminal-required = No Provider is configured; run `rivo tui` in an interactive terminal or use `rivo config provider set ...`
cli-tui-first-launch-workspace-invalid = Workspace does not exist or is not a directory: {$path}
cli-tui-first-launch-provider-saved = Provider saved and explicit discovery completed; choose a Vendor and Model next.
cli-tui-first-launch-provider-id = Provider ID
cli-tui-first-launch-base-url = Base URL
cli-tui-first-launch-credential = Existing Credential name
cli-tui-first-launch-discover = Save and discover models now (yes/no)
cli-tui-first-launch-title = Rivo · First-launch setup
cli-tui-first-launch-workspace = Workspace: {$path}
cli-tui-first-launch-help = Enter next/confirm final field · Tab/↑↓ switch · Esc cancel
cli-tui-first-launch-cancelled = First-launch setup cancelled; configuration was not saved
cli-tui-first-launch-not-confirmed = Save and discovery were not confirmed; configuration was not saved
cli-tui-first-launch-discovery-failed = Provider saved, but model discovery failed; check the credential reference and run refresh explicitly
cli-tui-first-launch-provider-exists = Provider already exists; update it through a controlled configuration change

cli-workspace-added = workspace_id={$id}
    name={$name}
    path={$path}
cli-workspace-opened = opened workspace {$name} ({$path})
cli-workspace-renamed = renamed workspace {$id}
cli-workspace-rebound = rebound workspace {$id} to {$path}
cli-workspace-removed = removed workspace {$id} and its sessions
cli-workspace-storage-worker-interrupted = Local session storage worker was interrupted
cli-workspace-id = ID
cli-workspace-name = Name
cli-workspace-path = Path
cli-workspace-default = Default
cli-workspace-last-opened = Last opened
cli-workspace-rename-usage = Usage: rivo workspace rename <PATH|WORKSPACE_ID> <NAME>
cli-workspace-rebind-usage = Usage: rivo workspace rebind <WORKSPACE_ID> <PATH>
cli-workspace-remove-usage = Usage: rivo workspace remove <PATH|WORKSPACE_ID> --yes
cli-workspace-usage = Usage: rivo workspace list|add|open|show|rename|rebind|remove
cli-workspace-format-value = --format requires a value
cli-workspace-list-arg = Unknown workspace list argument: {$value}
cli-workspace-format-invalid = Invalid output format: {$value}
cli-workspace-add-usage = Usage: rivo workspace add <PATH> [--name <NAME>]
cli-workspace-one-arg-usage = Usage: rivo workspace {$command} <PATH|WORKSPACE_ID>
cli-diagnostics-created = created {$path}
cli-diagnostics-usage = Usage: rivo diagnostics create <path> [--yes]
cli-diagnostics-confirm = The diagnostic bundle may contain redacted user business content. Continue export? [y/N]
cli-diagnostics-cancelled = Diagnostic export cancelled; automation must pass --yes
cli-diagnostics-storage-worker-interrupted = Local session storage worker was interrupted
cli-instance-data-directory-error = Could not determine the instance data directory
cli-instance-skill-missing = Default process-control Skill was not loaded
cli-instance-usage = Usage: rivo instance [list|endpoint|skill]
cli-instance-list-help = List registered local instances as JSON.

    Usage: rivo instance list
cli-instance-endpoint-help = Show the local instance-control endpoint.

    Usage: rivo instance endpoint
cli-instance-skill-help = Show the built-in process-control Skill.

    Usage: rivo instance skill
cli-upgrade-help = Check the latest CLI archive for this platform; V1 upgrades are manual archive replacements.

    Usage: rivo upgrade [OPTIONS]

    Options:
          --check  Compare versions without downloading or installing
      -h, --help   Show help
cli-app-server-help = Start the local JSON-RPC bridge.

    Usage: rivo app-server <--stdio|--pipe>

    Options:
          --stdio  Transport JSON-RPC over stdin/stdout
          --pipe   Serve locally over a Windows named pipe
      -h, --help   Show help
cli-language-confirm-rollback = Rolling back a language file requires explicit --yes
cli-language-confirm-install = Language file already exists: {$path}; replacing it requires explicit --yes
cli-language-confirm-remove = Removing a language file requires explicit --yes
cli-language-confirm-update = Updating a language file requires explicit --yes
cli-language-registry-missing = Registry has no language {$locale}
cli-language-destination-missing = Language installation destination is missing
cli-language-async-command = Remote language commands must use the asynchronous entry point
cli-upgrade-current-version = Current version: {$version}
cli-upgrade-latest-version = Latest version: {$version}
cli-upgrade-current = rivo is already up to date.
cli-upgrade-available = An update is available.
cli-upgrade-downgrade = Remote version {$remote} is lower than current version {$current}; downgrade rejected
cli-upgrade-installed = Rivo {$version} installed. Restart the CLI/TUI process.
cli-upgrade-platform-unsupported = rivo upgrade does not support platform: {$platform}
cli-upgrade-arch-unsupported = rivo upgrade does not support architecture: {$arch}
cli-upgrade-http-client-failed = Failed to create the upgrade HTTP client
cli-upgrade-manifest-read-failed = Failed to read latest manifest: {$url}
cli-upgrade-checksums-read-failed = Failed to read SHA256SUMS: {$url}
cli-upgrade-http-request-failed = HTTP request failed
cli-upgrade-http-response-read-failed = Failed to read the HTTP response
cli-upgrade-json-parse-failed = Failed to parse the JSON response
cli-upgrade-schema-unsupported = Unsupported release manifest schema: {$schema}
cli-upgrade-product-mismatch = Release manifest product does not match: {$product}
cli-upgrade-target-mismatch = Release manifest target does not match: {$manifest_platform}/{$manifest_arch}; current is {$platform}/{$arch}
cli-upgrade-artifact-missing = Latest manifest has no package for the current platform
cli-upgrade-artifact-path-invalid = Package filename must not contain path separators: {$file}
cli-upgrade-checksum-invalid = Package checksum is not a valid SHA-256: {$file}
cli-upgrade-checksum-entry-missing = SHA256SUMS does not declare the package from the manifest: {$file}
cli-upgrade-download-failed = Failed to download the package
cli-upgrade-download-http-failed = Package download returned HTTP {$status}
cli-upgrade-package-read-failed = Failed to read the package
cli-upgrade-checksum-mismatch = Package checksum mismatch: expected {$expected}, got {$actual}
cli-upgrade-temp-write-failed = Failed to write temporary package: {$path}
cli-upgrade-semver-invalid = Version is not a supported SemVer: {$value}
cli-sessions-path-help = Show the local session database path.

    Usage: rivo sessions path
cli-sessions-backup-help = Back up the current database without modifying the source.

    Usage: rivo sessions backup <target>
cli-sessions-reset-help = Rebuild schema 14 sessions as schema 18 after validating a backup.

    Usage: rivo sessions reset --backup <target> --yes
cli-sessions-migrate-help = Back up, validate and migrate schema 17 to schema 18 Workspace history.

    Usage: rivo sessions migrate --yes
    rivo sessions migrate --backup <target> --yes
cli-sessions-list-help = List local sessions.

    Usage: rivo sessions list
cli-sessions-show-help = Show session events and messages.

    Usage: rivo sessions show <SESSION_ID>
cli-sessions-resume-help = Resume a session in its matching Workspace and submit a new task.

    Usage: rivo sessions resume <SESSION_ID> (--prompt <TEXT>|--stdin) [--workspace <PATH|WORKSPACE_ID>]
cli-sessions-attach-help = Explicitly attach an unscoped legacy session to a Workspace.

    Usage: rivo sessions attach <SESSION_ID> <PATH|WORKSPACE_ID>
cli-sessions-delete-help = Delete a local session.

    Usage: rivo sessions delete <SESSION_ID> [--yes]
cli-sessions-clear-help = Clear all local session history.

    Usage: rivo sessions clear [--yes]
cli-sessions-backup-created = backup created
    source={$source}
    destination={$destination}
    schema_version={$schema}
    sha256={$sha256}
cli-sessions-reset-complete = reset complete: schema 18 session history created; the schema 14 backup is retained
cli-sessions-migration-current = migration not needed: schema 18 is already current
cli-sessions-migration-complete = migrate complete: schema 17 -> schema 18
    backup={$backup}
    backup_sha256={$sha256}
    bound_sessions={$bound}
    unscoped_sessions={$unscoped}
    backup retained
cli-sessions-migration-complete-simple = migrate complete: schema 18 Workspace session history created; backup retained
cli-sessions-deleted = deleted {$id}
cli-sessions-cleared = cleared local session history
cli-sessions-attached = attached session {$id}
cli-sessions-expired = expired: {$reason}
cli-sessions-restorable = restorable
cli-sessions-no-persisted-task = No persisted task
cli-sessions-retention-policy = local retention policy
cli-sessions-column-id = ID
cli-sessions-column-workspace-id = Workspace ID
cli-sessions-column-workspace = Workspace
cli-sessions-column-workspace-path = Workspace Path
cli-sessions-column-created = Created
cli-sessions-column-summary = Summary
cli-sessions-column-events = Events
cli-sessions-column-messages = Messages
cli-sessions-column-status = Status
cli-sessions-column-type = Type
cli-sessions-column-sequence = Sequence
cli-sessions-column-kind = Kind
cli-sessions-confirm-delete = Delete local session {$id}
cli-sessions-confirm-clear = Clear all local session history
cli-sessions-workspace-value-missing = --workspace requires a value
cli-sessions-unknown-list-arg = unknown sessions list argument: {$value}
cli-sessions-scope-exclusive = --workspace, --all-workspaces and --unscoped are mutually exclusive
cli-sessions-storage-worker-interrupted = local session storage worker was interrupted
cli-sessions-backup-usage = Usage: rivo sessions backup <target>
cli-sessions-reset-usage = Usage: rivo sessions reset --backup <target> --yes
cli-sessions-migrate-usage = Usage: rivo sessions migrate --yes|--backup <target> --yes
cli-sessions-show-usage = Usage: rivo sessions show <session_id>
cli-sessions-delete-usage = Usage: rivo sessions delete <session_id> [--yes]
cli-sessions-clear-usage = Usage: rivo sessions clear [--yes]
cli-sessions-resume-usage = Usage: rivo sessions resume <session_id> (--prompt <TEXT>|--stdin)
cli-sessions-stdin-empty = --stdin did not receive a non-empty UTF-8 task
cli-sessions-prompt-empty = --prompt must provide non-empty task text
cli-sessions-attach-usage = Usage: rivo sessions attach <session_id> <PATH|WORKSPACE_ID>
cli-sessions-usage = Usage: rivo sessions path|backup <target>|reset --backup <target> --yes|migrate --yes|migrate --backup <target> --yes|list [--workspace <path|id>|--all-workspaces|--unscoped]|show <id>|resume <id> (--prompt <TEXT>|--stdin) [--workspace <path|id>]|attach <id> <path|id>|delete <id> [--yes]|clear [--yes]
cli-sessions-prompt-value-missing = --prompt requires a value
cli-sessions-prompt-duplicate = --prompt may only be specified once
cli-sessions-stdin-duplicate = --stdin may only be specified once
cli-sessions-resume-usage-workspace = Usage: rivo sessions resume <session_id> (--prompt <TEXT>|--stdin) [--workspace <path|id>]
cli-sessions-prompt-or-stdin = exactly one of --prompt or --stdin is required
cli-sessions-events = events
cli-sessions-messages = messages
cli-run-media-ready-path = rivo: media ready: {$path}
cli-run-media-ready-url = rivo: media ready: {$url}
cli-run-media-started = rivo: media started: {$operation}
cli-run-media-failed = rivo: media failed: {$message}
cli-run-approval-unavailable = rivo: approval prompt is unavailable without a TTY; request denied
cli-run-approval-denied = rivo: approval denied for {$tool}; use TTY prompt or an exact approval manifest rule
cli-run-interaction-noninteractive = rivo: non-interactive mode cannot answer interaction {$id}
cli-run-stopped = rivo: run stopped: {$reason}
cli-run-stream-ended = rivo: session event stream ended without a terminal event
cli-run-shutdown-failed = rivo: session shutdown failed: {$error}
cli-mcp-not-found = MCP server not found
cli-mcp-login-oauth-only = rivo mcp login only supports HTTP servers configured with --oauth
cli-mcp-login-success = MCP server {$name} login succeeded; credentials stored in the system keychain
cli-mcp-disabled = MCP server is disabled: {$name}
cli-mcp-state-changed = MCP server {$name} {$action}d
cli-mcp-install-ready = installed MCP server {$name}; status=ready tools={$tools} resources={$resources} prompts={$prompts}
cli-mcp-install-oauth-pending = installed MCP server {$name} (OAuth not logged in; live discovery skipped); run `rivo mcp login {$name}`, then `rivo mcp inspect {$name}` to verify
cli-mcp-toolset-missing-tool = MCP server {$server} toolset {$toolset} references undiscovered tool {$tool}
cli-config-path-unresolved = Cannot determine configuration file path
cli-mcp-name-invalid = MCP server name cannot be empty or contain whitespace
cli-mcp-oauth-http-only = --oauth is only supported for HTTP MCP
cli-mcp-loopback-http-only = --allow-loopback is only supported for HTTP MCP
cli-mcp-auto-login-http-only = --auto-login is only supported for HTTP MCP
cli-mcp-flag-value-missing = {$flag} requires a value
cli-mcp-capability-unknown = Unknown MCP capability: {$capability}
cli-mcp-toolset-risk-invalid = toolset risk must be read_only, action or unknown
cli-mcp-toolset-format-invalid = toolset format is <name>:<risk>:<tool,tool>
cli-mcp-header-env-format-invalid = --header-env format is <Header>=<ENV>
cli-mcp-env-format-invalid = --env format is <KEY>=<ENV>
cli-mcp-bearer-env-missing = --bearer-env requires an environment variable name
cli-mcp-oauth-token-env-missing = --oauth-token-env requires an environment variable name
cli-mcp-oauth-port-invalid = --oauth-redirect-port must be a port number
cli-mcp-install-flag-unsupported = Unsupported MCP install argument: {$flag}
cli-mcp-capability-required = At least one --capability must be declared
cli-mcp-toolset-required = at least one exact --toolset is required when declaring tools capability
cli-mcp-auth-conflict = --bearer-env and --oauth cannot be used together
cli-mcp-stdio-header-env-unsupported = stdio MCP does not support --header-env; provide credentials through the process environment
cli-mcp-transport-invalid = MCP install transport must be http or stdio
cli-mcp-config-invalid = Invalid MCP configuration output
cli-mcp-usage = Usage: rivo mcp list|show|inspect <server>|install|enable|disable|remove
cli-output-property = Property
cli-output-value = Value
cli-output-type = Type
cli-output-name = Name
cli-output-description = Description
cli-output-config-path-unresolved = Unable to determine the configuration file path
cli-output-table-column-mismatch = Table row width does not match the header
cli-output-format-duplicate = --format cannot be repeated
cli-output-format-missing = --format requires a value
cli-mcp-root-help = Manage configured MCP servers.

    Usage: rivo mcp <COMMAND>

    Commands: list, show, inspect, login, install, enable, disable, remove

    Options: -h, --help  Show help
cli-mcp-list-help = List configured MCP servers.

    Usage:
      rivo mcp list
cli-mcp-show-help = Show a redacted MCP server configuration.

    Usage:
      rivo mcp show <NAME>
cli-mcp-inspect-help = Connect to an MCP server and discover tools, resources and prompts.

    Usage:
      rivo mcp inspect <NAME>
cli-mcp-login-help = Perform OAuth login for an HTTP MCP server.

    Usage:
      rivo mcp login <NAME>
cli-mcp-enable-help = Enable an MCP server.

    Usage:
      rivo mcp enable <NAME>
cli-mcp-disable-help = Disable an MCP server.

    Usage:
      rivo mcp disable <NAME>
cli-mcp-remove-help = Remove an MCP server.

    Usage:
      rivo mcp remove <NAME>
cli-mcp-install-help = Install and verify an MCP server.

    Usage:
      rivo mcp install <http|stdio> <NAME> <TARGET> [OPTIONS]

    Commands:
      http   Install a Streamable HTTP server
      stdio  Install a local stdio server
cli-mcp-install-http-help = Install a Streamable HTTP MCP server.

    Usage:
      rivo mcp install http <NAME> <HTTPS_URL> [OPTIONS]

    Options:
      --capability <LIST>     Allowed capabilities
      --toolset <SPEC>        Named toolset; repeatable
      --header-env <H>=<ENV>  Read an HTTP header from an environment variable
      --bearer-env <ENV>      Read a Bearer token from an environment variable
      --oauth                 Enable OAuth
      --allow-loopback        Allow a loopback URL
      --allow-sampling        Allow controlled sampling
cli-mcp-install-stdio-help = Install a local stdio MCP server.

    Usage:
      rivo mcp install stdio <NAME> <PROGRAM> [OPTIONS]

    Options:
      --arg <ARG>           Process argument; repeatable
      --cwd <PATH>          Process working directory
      --env <KEY>=<ENV>     Pass a process environment value
      --capability <LIST>   Allowed capabilities
      --toolset <SPEC>      Named toolset; repeatable
      --allow-sampling      Allow controlled sampling
cli-config-valid = valid
cli-config-updated = updated {$path}
cli-config-credential-saved = credential saved for {$name}
cli-config-credential-removed = credential removed for {$name}
cli-config-credentials-migrated = migrated {$count} credential(s)
cli-config-saved-revision = saved revision={$revision}
cli-config-discovery-refreshed = Model discovery refreshed: {$provider}
cli-config-removed-revision = removed revision={$revision}
cli-config-no-provider = no available provider for model={$model} vendor={$vendor}
cli-config-discovery-failed = Model discovery failed; check Provider connection and credential reference
cli-config-discovery-invalidated = Model discovery is stale; run `config provider models refresh {$provider}`
cli-config-model-state = model={$id}	{$state}
cli-config-credentials-help = Manage local credentials.

    Usage: rivo config credentials [COMMAND]

    Commands: path, show, set, unset, migrate

    Options: -h, --help  Show help
cli-config-path-help = Show the configuration file path.

    Usage: rivo config path

    Options: -h, --help  Show help
cli-config-show-help = Show the redacted effective configuration.

    Usage: rivo config show

    Options: -h, --help  Show help
cli-config-validate-help = Validate configuration and the model catalog.

    Usage: rivo config validate

    Options: -h, --help  Show help
cli-config-set-help = Set a configuration value.

    Usage: rivo config set <KEY> [VALUE]

    Options: -h, --help  Show help
cli-config-unset-help = Unset a configuration value.

    Usage: rivo config unset <KEY> [VALUE]

    Options: -h, --help  Show help
cli-config-route-help = Preview deterministic Vendor/Model routing without making a request.

    Usage: rivo config route --vendor <VENDOR> [--model <MODEL>] [--provider-id <PROVIDER>]

    Options: -h, --help  Show help
cli-config-vendor-help = Manage the local Vendor catalog.

    Usage: rivo config vendor <list|show|set|remove> [ARGS]

    Options: -h, --help  Show help
cli-config-model-help = Manage the canonical Model catalog.

    Usage: rivo config model <list|show|set|remove> [ARGS]

    Options: -h, --help  Show help
cli-config-provider-help = Manage Provider connections.

    Usage: rivo config provider <list|show|set|remove> [ARGS]

    Options: -h, --help  Show help
cli-config-provider-set-help = Manage Provider connections.

    Usage: rivo config provider set <ID> --base-url <URL> [--credential-name <NAME>] [--enabled <BOOL>] [--priority <N>] [--refresh] [--confirm <TOKEN>]

    Options: -h, --help  Show help
cli-config-credentials-path-help = Show the credential file path.

    Usage: rivo config credentials path

    Options: -h, --help  Show help
cli-config-credentials-show-help = List credential names and saved state without showing secrets.

    Usage: rivo config credentials show

    Options: -h, --help  Show help
cli-config-credentials-set-help = Save a credential interactively or from an environment variable.

    Usage: rivo config credentials set <NAME> [--from-env <ENV>]

    Options: -h, --help  Show help
cli-config-credentials-unset-help = Delete a credential.

    Usage: rivo config credentials unset <NAME>

    Options: -h, --help  Show help
cli-config-credentials-migrate-help = Migrate legacy plaintext credentials to encrypted storage.

    Usage: rivo config credentials migrate

    Options: -h, --help  Show help
cli-config-credentials-set-usage = Usage: rivo config credentials set <environment-variable>
cli-config-credentials-from-env-usage = Usage: rivo config credentials set <name> --from-env <environment-variable>
cli-config-credentials-noninteractive = Cannot read an API key from non-interactive input; use --from-env <environment-variable>
cli-config-credentials-mismatch = API key entries do not match
cli-config-credentials-empty = API key cannot be empty
cli-config-credentials-unset-usage = Usage: rivo config credentials unset <environment-variable>
cli-config-credentials-in-use = Credential {$name} is still referenced by Provider(s): {$providers}; fix those Providers first
cli-config-credentials-usage = Usage: rivo config credentials path|show|set|unset|migrate
cli-config-model-providers-usage = Usage: rivo config model providers <model-id>
cli-config-provider-models-usage = Usage: rivo config provider models list|remote|refresh <provider-id>
cli-config-provider-not-found = Provider not found: {$id}
cli-config-vendor-not-found = Vendor not found: {$id}
cli-config-model-not-found = Model not found: {$id}
cli-config-show-id-missing = show requires an ID
cli-config-route-usage = Usage: rivo config route --vendor <vendor> [--model <model>] [--provider-id <provider>]
cli-output-field = Field
cli-config-set-usage = Usage: rivo config {$command} <key> [value]
cli-config-value-missing = Missing value for {$key}
cli-config-value-invalid = Invalid value for {$key}: {$error}
cli-config-unsupported-field = Unsupported configuration field: {$key}
cli-config-usage = Usage: rivo config path|show|validate|set|unset|vendor|model|provider|credentials|route
cli-config-locale-missing = Missing locale
cli-config-locale-invalid = Invalid locale: {$error}
cli-config-mcp-json-missing = Missing MCP server JSON
cli-config-mcp-json-invalid = Invalid MCP server JSON
cli-config-mcp-name-missing = Missing MCP server name
cli-config-credential-not-found = Credential {$name} does not exist; run `config credentials set {$name}` first
cli-config-model-discovery-failed = Model discovery failed; check credential reference and run refresh explicitly
cli-config-no-compatible-models = Model discovery succeeded but no discovered model is in the local model catalog
cli-config-env-missing = Environment variable {$name} is not set
cli-a2a-serve-usage = Usage: rivo a2a serve [<server-profile>] --config <owner-a2a-server.toml>
cli-a2a-transport-invalid = A2A transport must be mtls or psk
cli-a2a-server-args-invalid = Invalid A2A Server argument
cli-a2a-config-missing = A2A Server configuration path is required
cli-a2a-psk-secret-required = PSK transport requires --secret-stdin
cli-a2a-mtls-secret-forbidden = mTLS transport does not accept --secret-stdin
cli-a2a-psk-transport-required = --secret-stdin requires --transport psk
cli-a2a-secret-invalid = PSK secret must be 64 hexadecimal characters
cli-a2a-secret-length-invalid = PSK secret must be exactly 32 bytes
cli-a2a-secret-zero = PSK secret cannot be all zero
cli-a2a-pairings-usage = Usage: rivo a2a pairings list --config <owner-a2a-server.toml>|pairings approve|reject <request-id>|revoke <peer-id> --config <owner-a2a-server.toml>
cli-a2a-approvals-usage = Usage: rivo a2a approvals list --config <owner-a2a-server.toml>|approvals approve|reject <approval-id> --config <owner-a2a-server.toml>
cli-a2a-owner-terminal-required = A2A approval must run in an interactive owner session on the Server host
cli-a2a-pairing-cancelled = A2A pairing decision cancelled
cli-a2a-approval-cancelled = A2A local approval decision cancelled
cli-a2a-delegate-usage = Usage: rivo a2a delegate <peer-id> <skill-id> --message-stdin --yes [--stream]
cli-a2a-peer-skill-invalid = A2A peer_id or skill_id is invalid
cli-a2a-duplicate-flag = Duplicate A2A argument: {$flag}
cli-a2a-unknown-flag = Unknown A2A argument: {$flag}
cli-a2a-delegate-confirm-required = A2A delegation requires --message-stdin and explicit --yes confirmation for external data
cli-a2a-control-usage = Usage: rivo a2a control <peer-id> <target-instance-id> <method> --params-stdin [--yes]
cli-a2a-control-id-invalid = A2A control identifier or method is invalid
cli-a2a-control-method-forbidden = process-control does not allow this method
cli-a2a-control-params-required = A2A control requires --params-stdin
cli-a2a-control-confirm-required = Mutating A2A control requires --yes
cli-a2a-recover-usage = Usage: rivo a2a recover <peer-id> <task-id> [--transport psk --secret-stdin]
cli-a2a-peer-task-invalid = A2A peer_id or task_id is invalid
cli-a2a-cancel-usage = Usage: rivo a2a cancel <peer-id> <task-id> --yes [--transport psk --secret-stdin]
cli-a2a-pair-usage = Usage: rivo a2a pair <local-identity-label> --endpoint <https-url> --spki-sha256 <sha256>
cli-a2a-local-identity-missing = Local identity label is required
cli-a2a-identity-invalid = A2A identity label is invalid
cli-a2a-pair-args-invalid = Invalid A2A pair argument
cli-a2a-endpoint-missing = A2A endpoint is required
cli-a2a-spki-missing = A2A SPKI is required
cli-a2a-sas-terminal-required = A2A pairing requires interactive local SAS confirmation
cli-a2a-pair-cancelled = A2A pairing cancelled
cli-a2a-profile-required = Multiple Server profiles are configured; specify `rivo a2a serve <server-profile> --config <path>`
cli-a2a-bootstrap-secret-missing = PSK Server requires an in-memory bootstrap secret
cli-a2a-server-tls-read-failed = Cannot read the A2A Server TLS identity from the local keychain; private keys are not accepted in config or SQLite
cli-a2a-workspace-alias-invalid = A2A workspace alias cannot be resolved: {$alias}

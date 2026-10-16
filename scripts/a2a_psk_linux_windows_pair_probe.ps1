[CmdletBinding()]
param(
    [string]$RemoteHost = "192.168.8.11",
    [string]$RemoteBinary = "/tmp/rivo-a2a-src/runtime/target/release/rivo",
    [string]$WindowsBinary = "runtime/target/release/rivo.exe",
    [string]$Bind = "192.168.8.30:8443",
    [switch]$OpenFirewall
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($RemoteHost -notmatch '^[A-Za-z0-9.-]+$') {
    throw "invalid_remote_host"
}
if ($RemoteBinary -notmatch '^/[A-Za-z0-9._/-]+$') {
    throw "invalid_remote_binary_path"
}
if ($Bind -notmatch '^[0-9.]+:[0-9]{1,5}$') {
    throw "invalid_bind"
}

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$binary = (Resolve-Path (Join-Path $root $WindowsBinary)).Path
& ssh -o BatchMode=yes "root@$RemoteHost" "test -x '$RemoteBinary'"
if ($LASTEXITCODE -ne 0) {
    throw "remote_binary_missing path=$RemoteBinary host=$RemoteHost; build or copy the Debian release binary first"
}
$runRoot = Join-Path $root (".a2a-psk-reverse-" + [guid]::NewGuid().ToString("N"))
$configPath = Join-Path $runRoot "server.toml"
$workspacePath = Join-Path $runRoot "workspace"
$dataPath = Join-Path $runRoot "data"
$firewallRule = "Rivo A2A PSK smoke " + [guid]::NewGuid().ToString("N")
$firewallRuleCreated = $false
$port = [int]($Bind.Split(':')[-1])
$secretBytes = New-Object byte[] 32
[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($secretBytes)
$secret = ([BitConverter]::ToString($secretBytes)).Replace("-", "").ToLowerInvariant()

function Write-ServerConfig([string[]]$AllowedPeers) {
    $peers = ($AllowedPeers | ForEach-Object { '"' + $_ + '"' }) -join ", "
    @"
schema_version = 1
state_db = "state.db"

[workspace_aliases]
research = "workspace"

[[profiles]]
identity_ref = "a2a-test-psk-ephemeral"

[profiles.profile]
name = "research-lan"
bind = "$Bind"
skill_id = "research"
workspace_alias = "research"
capability_profile = "research"
policy_ceiling = "strict"
network_enabled = false
browser_enabled = false
mcp_servers = []
mcp_toolsets = {}
per_peer_max_active_tasks = 1
max_active_tasks = 3
max_message_bytes = 65536
max_output_bytes = 262144
max_duration_seconds = 1800
approval_timeout_seconds = 300
allowed_peers = [$peers]
issuer_ref = "a2a-test-psk-ephemeral-issuer"
idempotency_retention_seconds = 86400
audit_retention_days = 30
"@ | Set-Content -LiteralPath $configPath -Encoding ascii
}

function Start-PskServer {
    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $binary
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.Environment["RIVO_DATA_DIR"] = $dataPath
    $info.Environment["RIVO_A2A_SMOKE_HOLD_MS"] = "30000"
    $info.Arguments = "a2a serve research-lan --config `"$configPath`" --transport psk --secret-stdin"
    $process = [System.Diagnostics.Process]::Start($info)
    $process.StandardInput.WriteLine($secret)
    $process.StandardInput.Close()
    $startupRead = $process.StandardOutput.ReadLineAsync()
    if (-not $startupRead.Wait(3000)) {
        $process.Kill()
        $process.WaitForExit()
        $error = $process.StandardError.ReadToEnd()
        throw "windows_server_start_timeout $error"
    }
    $startup = $startupRead.Result
    if ($startup -notmatch 'spki_sha256=([0-9a-f]{64})') {
        $error = $process.StandardError.ReadToEnd()
        throw "windows_server_start_failed $error"
    }
    [pscustomobject]@{ Process = $process; Spki = $matches[1] }
}

function Invoke-RemotePsk([string]$Command, [string[]]$InputLines) {
    $info = [System.Diagnostics.ProcessStartInfo]::new()
    $info.FileName = "ssh"
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    # Windows PowerShell 5.1 lacks ProcessStartInfo.ArgumentList. All values
    # interpolated here are fixed CLI fragments or validated before this call.
    $info.Arguments = "-o BatchMode=yes root@$RemoteHost `"RIVO_DATA_DIR=/tmp/rivo-psk-reverse-client $RemoteBinary $Command`""
    $process = [System.Diagnostics.Process]::Start($info)
    foreach ($line in $InputLines) { $process.StandardInput.WriteLine($line) }
    $process.StandardInput.Close()
    if (-not $process.WaitForExit(12000)) {
        $process.Kill()
        $process.WaitForExit()
        throw "linux_client_timeout"
    }
    [pscustomobject]@{
        ExitCode = $process.ExitCode
        Stdout = $process.StandardOutput.ReadToEnd()
        Stderr = $process.StandardError.ReadToEnd()
    }
}

function Stop-PskServer($Server) {
    if ($null -ne $Server -and -not $Server.Process.HasExited) {
        $Server.Process.Kill()
        $Server.Process.WaitForExit()
    }
}

$first = $null
$server = $null
try {
    New-Item -ItemType Directory -Force -Path $workspacePath, $dataPath | Out-Null
    if ($OpenFirewall) {
        try {
            New-NetFirewallRule -DisplayName $firewallRule -Direction Inbound -Action Allow `
                -Protocol TCP -LocalPort $port -RemoteAddress $RemoteHost -Profile Private `
                -ErrorAction Stop | Out-Null
        } catch {
            throw "firewall_rule_failed access_denied_or_policy; run this script from an elevated Administrator PowerShell"
        }
        $firewallRuleCreated = $true
    }
    Write-ServerConfig @()
    $first = Start-PskServer
    $firstSpki = $first.Spki
    $cardHeaders = curl.exe -ksS --connect-timeout 3 --max-time 3 -D - -o NUL "https://$Bind/.well-known/agent-card.json"
    if ($LASTEXITCODE -ne 0) { throw "windows_server_card_unreachable" }
    $peerId = ($cardHeaders | Select-String -Pattern '^x-rivo-psk-peer-id:\s*(peer-[0-9a-f]{32})' -CaseSensitive:$false).Matches.Groups[1].Value
    if ($peerId -notmatch '^peer-[0-9a-f]{32}$') { throw "windows_server_psk_peer_id_missing" }
    Stop-PskServer $first
    $first = $null

    Write-ServerConfig @($peerId)
    $server = Start-PskServer
    if ($server.Spki -ne $firstSpki) { throw "psk_spki_changed_after_restart" }
    & ssh -o BatchMode=yes "root@$RemoteHost" "mkdir -p /tmp/rivo-psk-reverse-client/rivo"
    if ($LASTEXITCODE -ne 0) { throw "remote_client_data_dir_failed" }
    $pair = Invoke-RemotePsk "a2a pair reverse --endpoint https://$Bind/a2a --spki-sha256 $($server.Spki) --transport psk --secret-stdin" @($secret)
    if ($pair.ExitCode -ne 0 -or $pair.Stdout -notmatch 'transport=psk') {
        throw "linux_client_pair_failed $($pair.Stderr)"
    }
    Write-Output "reverse_pair_probe=passed peer_id=$peerId transport=psk"

    $delegate = Invoke-RemotePsk "a2a delegate $peerId research --message-stdin --yes --transport psk --secret-stdin" @($secret, "reverse lifecycle smoke")
    $taskId = [regex]::Match($delegate.Stdout, 'task_id=(task-[0-9a-f]+)').Groups[1].Value
    if ($delegate.ExitCode -ne 0 -or $taskId -notmatch '^task-[0-9a-f]+$' -or $delegate.Stdout -notmatch 'state=Working') {
        throw "reverse_delegate_failed $($delegate.Stderr)"
    }
    foreach ($attempt in 1..2) {
        $recover = Invoke-RemotePsk "a2a recover $peerId $taskId --transport psk --secret-stdin" @($secret)
        if ($recover.ExitCode -ne 0 -or $recover.Stdout -notmatch "task_id=$taskId state=Working") {
            throw "reverse_recover_failed attempt=$attempt $($recover.Stderr)"
        }
    }
    $cancel = Invoke-RemotePsk "a2a cancel $peerId $taskId --yes --transport psk --secret-stdin" @($secret)
    if ($cancel.ExitCode -ne 0 -or $cancel.Stdout -notmatch "task_id=$taskId state=Canceled") {
        throw "reverse_cancel_failed $($cancel.Stderr)"
    }
    $revoke = Invoke-RemotePsk "a2a revoke $peerId --yes" @()
    if ($revoke.ExitCode -ne 0) { throw "reverse_revoke_failed $($revoke.Stderr)" }
    $afterRevoke = Invoke-RemotePsk "a2a delegate $peerId research --message-stdin --yes --transport psk --secret-stdin" @($secret, "post revoke")
    $afterRevokeText = $afterRevoke.Stdout + $afterRevoke.Stderr
    if ($afterRevoke.ExitCode -eq 0 -or ($afterRevokeText -notmatch 'a2a_|peer')) {
        throw "reverse_revoke_not_enforced"
    }
    Write-Output "reverse_lifecycle_probe=passed peer_id=$peerId task_id=$taskId states=WORKING,RECOVERED_TWICE,CANCELED revoke=post_revoke_delegate_rejected"
} finally {
    Stop-PskServer $first
    Stop-PskServer $server
    if ($firewallRuleCreated) {
        Remove-NetFirewallRule -DisplayName $firewallRule -ErrorAction SilentlyContinue
    }
    & ssh -o BatchMode=yes "root@$RemoteHost" "rm -rf /tmp/rivo-psk-reverse-client" 2>$null
    if (Test-Path -LiteralPath $runRoot) { Remove-Item -LiteralPath $runRoot -Recurse -Force }
}

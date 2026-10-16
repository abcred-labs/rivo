[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ServerConfig,
    [Parameter(Mandatory = $true)]
    [string]$ServerProfile,
    [Parameter(Mandatory = $true)]
    [string]$ServerBind,
    [string]$rivo = "",
    [string]$ClientDataDir = "",
    [Parameter(Mandatory = $true)]
    [string]$ServerIdentityRef,
    [Parameter(Mandatory = $true)]
    [string]$IssuerRef,
    [ValidateSet('mtls','psk')]
    [string]$Transport = 'mtls',
    [switch]$KeepRunning
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Fail([string]$Message) { throw "A2A local smoke: $Message" }

if (-not (Test-Path -LiteralPath $ServerConfig -PathType Leaf)) {
    Fail "Server config was not found: $ServerConfig"
}

try {
    $separator = $ServerBind.LastIndexOf(':')
    if ($separator -le 0 -or $separator -eq ($ServerBind.Length - 1)) { throw 'missing port' }
    $bindAddress = [System.Net.IPAddress]::Parse($ServerBind.Substring(0, $separator))
    $bindPort = [int]::Parse($ServerBind.Substring($separator + 1))
    $bind = [System.Net.IPEndPoint]::new($bindAddress, $bindPort)
} catch {
    Fail "ServerBind must be an explicit IP:port, for example 192.168.1.20:8443"
}

$octets = $bind.Address.GetAddressBytes()
$privateV4 = $octets.Length -eq 4 -and (
    $octets[0] -eq 10 -or
    ($octets[0] -eq 172 -and $octets[1] -ge 16 -and $octets[1] -le 31) -or
    ($octets[0] -eq 192 -and $octets[1] -eq 168)
)
if ($bind.Address.AddressFamily -ne [System.Net.Sockets.AddressFamily]::InterNetwork -or -not $privateV4) {
    Fail "ServerBind must be an RFC1918 IPv4 address; loopback, wildcard, DNS and public addresses are rejected"
}
if ($bind.Port -eq 0) { Fail 'ServerBind port must be non-zero' }

if ([string]::IsNullOrWhiteSpace($rivo)) {
    $command = Get-Command rivo -ErrorAction SilentlyContinue
    if ($null -eq $command) { Fail 'rivo was not found; pass -rivo <path-to-rivo.exe>' }
    $rivo = $command.Source
}
if (-not (Test-Path -LiteralPath $rivo -PathType Leaf)) {
    Fail "rivo executable was not found: $rivo"
}

if ([string]::IsNullOrWhiteSpace($ClientDataDir)) {
    $ClientDataDir = Join-Path ([System.IO.Path]::GetTempPath()) ("rivo-a2a-client-" + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Force -Path $ClientDataDir | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $ClientDataDir 'rivo') | Out-Null

$serverDataDir = Join-Path $ClientDataDir 'server-data'
New-Item -ItemType Directory -Force -Path $serverDataDir | Out-Null

Write-Host "A2A local smoke preflight passed."
Write-Host "  server_config=$ServerConfig"
Write-Host "  server_profile=$ServerProfile"
Write-Host "  server_bind=$ServerBind"
Write-Host "  isolated_data_dir=$ClientDataDir"
Write-Host "  transport=$Transport"

if ($Transport -eq 'psk') {
    Write-Host 'PSK mode requires a 32-byte secret on stdin for every serve/client invocation.'
    Write-Host 'The script never accepts or stores the secret; use a protected interactive pipe.'
}

& $rivo a2a provision check --server-identity-ref $ServerIdentityRef --issuer-ref $IssuerRef
if ($LASTEXITCODE -ne 0) {
    Fail "A2A keychain provisioning check failed; listener smoke was not started"
}
Write-Host 'A2A keychain provisioning check passed.'

$env:RIVO_DATA_DIR = $serverDataDir
$serverArgs = @('a2a', 'serve', $ServerProfile, '--config', $ServerConfig)
if ($Transport -eq 'psk') { $serverArgs += @('--transport', 'psk', '--secret-stdin') }
$server = Start-Process -FilePath $rivo -ArgumentList $serverArgs -PassThru -NoNewWindow
try {
    Start-Sleep -Seconds 2
    $server.Refresh()
    if ($server.HasExited) {
        Fail "A2A Server exited during startup (exit $($server.ExitCode)); verify keychain materials, config and firewall"
    }

    Write-Host "A2A Server is running (PID $($server.Id))."
    Write-Host ''
    Write-Host 'Client/owner smoke sequence:'
    Write-Host "  `$env:RIVO_DATA_DIR='$ClientDataDir'"
    Write-Host "  & '$rivo' a2a discover"
    Write-Host "  & '$rivo' a2a pair <local-identity-label> --endpoint https://$ServerBind/a2a --spki-sha256 <sha256>"
    Write-Host "  & '$rivo' a2a pairings list --config '$ServerConfig'"
    Write-Host "  & '$rivo' a2a pairings approve <request-id> --config '$ServerConfig'"
    Write-Host "  & '$rivo' a2a inspect <peer-id>"
    if ($Transport -eq 'psk') {
        Write-Host "  <protected-stdin-producer> | & '$rivo' a2a pair <label> --endpoint https://$ServerBind/a2a --transport psk --secret-stdin"
        Write-Host "  <protected-stdin-producer> | & '$rivo' a2a delegate <peer-id> research --message-stdin --yes --stream --transport psk --secret-stdin"
        Write-Host "  <protected-stdin-producer> | & '$rivo' a2a recover <peer-id> <task-id> --transport psk --secret-stdin"
        Write-Host "  <protected-stdin-producer> | & '$rivo' a2a revoke <peer-id> --yes --transport psk --secret-stdin"
    } else {
        Write-Host "  & '$rivo' a2a delegate <peer-id> research --message-stdin --yes --stream"
        Write-Host "  & '$rivo' a2a recover <peer-id> <task-id>"
        Write-Host "  & '$rivo' a2a revoke <peer-id> --yes"
    }
    Write-Host ''
    Write-Host 'Record evidence for status transitions, cancellation, certificate/Card change rejection and redacted logs/SQLite.'
    if (-not $KeepRunning) {
        Read-Host 'Press Enter after the operator-assisted smoke sequence completes'
    }
} finally {
    if (-not $KeepRunning -and -not $server.HasExited) {
        Stop-Process -Id $server.Id -Force
        Write-Host "A2A Server stopped (PID $($server.Id))."
    }
    if (-not $KeepRunning) {
        & $rivo a2a provision cleanup --yes --server-identity-ref $ServerIdentityRef --issuer-ref $IssuerRef
    }
}

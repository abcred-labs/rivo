[CmdletBinding()]
param(
    [Alias('h')]
    [switch]$Help,
    [string]$Version = $(if ($env:RIVO_INSTALL_VERSION) { $env:RIVO_INSTALL_VERSION } else { 'latest' }),
    [string]$InstallRoot = $(if ($env:RIVO_INSTALL_ROOT) { $env:RIVO_INSTALL_ROOT } else { Join-Path $env:ProgramFiles 'Rivo' }),
    [string]$BaseUrl = $(if ($env:RIVO_INSTALL_BASE_URL) { $env:RIVO_INSTALL_BASE_URL } else { '' })
)

$ErrorActionPreference = 'Stop'
if ($Help) {
    @'
Install the latest stable Rivo release.

Parameters:
  -Version VERSION      Install VERSION instead of latest
  -InstallRoot PATH     Override the system install root
  -BaseUrl URL           Use a release mirror
  -Help                  Show this help
'@
    exit 0
}
$repository = if ($env:RIVO_REPOSITORY) { $env:RIVO_REPOSITORY } else { 'abcred-labs/rivo' }
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run this installer from an elevated PowerShell window.' }

$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($architecture -eq 'X64') { $arch = 'amd64' } elseif ($architecture -eq 'Arm64') { $arch = 'arm64' } else { throw "Unsupported Windows architecture: $architecture" }
if ($arch -ne 'amd64') { throw 'Windows release archives currently support amd64 only.' }

if ([string]::IsNullOrWhiteSpace($BaseUrl)) {
    if ($Version -eq 'latest') { $BaseUrl = "https://github.com/$repository/releases/latest/download" } else { $BaseUrl = "https://github.com/$repository/releases/download/v$Version" }
    $manifestUrl = "$BaseUrl/release-manifest-windows-amd64.json"
    $mirror = $false
} else {
    $BaseUrl = $BaseUrl.TrimEnd('/')
    $manifestUrl = "$BaseUrl/windows-amd64/release-manifest.json"
    $mirror = $true
}

$temp = Join-Path ([IO.Path]::GetTempPath()) ('rivo-install-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    $manifestPath = Join-Path $temp 'manifest.json'
    Invoke-WebRequest -UseBasicParsing -Uri $manifestUrl -OutFile $manifestPath
    $manifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
    if ($manifest.schema -ne 2 -or $manifest.product -ne 'rivo') { throw 'Unsupported Rivo release manifest.' }
    if ($manifest.channel -ne 'stable') { throw 'Release manifest is not a stable release.' }
    if ($manifest.platform -ne 'windows' -or $manifest.arch -ne 'amd64') { throw 'Release manifest target mismatch.' }
    if ($manifest.version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw 'Invalid release version.' }
    if ($Version -ne 'latest' -and $Version -ne $manifest.version) { throw "Requested version $Version does not match manifest $($manifest.version)." }
    $artifact = $manifest.artifacts[0]
    $expectedName = "rivo-$($manifest.version)-windows-amd64.zip"
    if ($artifact.file -ne $expectedName) { throw 'Release artifact does not match the target.' }
    $artifactBase = if ($mirror) { "$BaseUrl/windows-amd64" } else { $BaseUrl }
    $archive = Join-Path $temp $artifact.file
    $sidecar = "$archive.sha256"
    Invoke-WebRequest -UseBasicParsing -Uri "$artifactBase/$($artifact.file)" -OutFile $archive
    Invoke-WebRequest -UseBasicParsing -Uri "$artifactBase/$($artifact.file).sha256" -OutFile $sidecar
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
    $declared = ((Get-Content -Raw -LiteralPath $sidecar).Trim() -split '\s+')[0].ToLowerInvariant()
    if ($actual -ne $artifact.sha256.ToLowerInvariant() -or $actual -ne $declared) { throw 'Release checksum verification failed.' }

    $extract = Join-Path $temp 'extract'
    Expand-Archive -LiteralPath $archive -DestinationPath $extract
    $payload = Join-Path $extract ("rivo-$($manifest.version)-windows-amd64")
    if (-not (Test-Path -LiteralPath (Join-Path $payload 'rivo.exe'))) { throw 'Archive does not contain rivo.exe.' }
    if (-not (Test-Path -LiteralPath (Join-Path $payload 'rivo-browser-extension-host.exe'))) { throw 'Archive is missing the Windows extension host.' }
    if (-not (Test-Path -LiteralPath (Join-Path $payload 'share\rivo\languages\en-US.ftl'))) { throw 'Archive is missing language resources.' }

    if ($InstallRoot -eq [IO.Path]::GetPathRoot($InstallRoot)) { throw "Refusing unsafe install root: $InstallRoot" }
    $versions = Join-Path $InstallRoot 'versions'
    New-Item -ItemType Directory -Force -Path $versions | Out-Null
    $versionDir = Join-Path $versions $manifest.version
    if (Test-Path -LiteralPath $versionDir) { Remove-Item -Recurse -Force -LiteralPath $versionDir }
    $current = Join-Path $InstallRoot 'current'
    $previous = Join-Path $InstallRoot 'previous'
    $switched = $false
    try {
        Move-Item -LiteralPath $payload -Destination $versionDir
        if (Test-Path -LiteralPath $current) {
            if (Test-Path -LiteralPath $previous) { Remove-Item -Recurse -Force -LiteralPath $previous }
            Move-Item -LiteralPath $current -Destination $previous
        }
        Move-Item -LiteralPath $versionDir -Destination $current
        $switched = $true
    } catch {
        if (-not $switched -and (Test-Path -LiteralPath $previous) -and -not (Test-Path -LiteralPath $current)) {
            Move-Item -LiteralPath $previous -Destination $current -ErrorAction SilentlyContinue
        }
        throw
    }
    Move-Item -LiteralPath $versionDir -Destination $current
    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $pathEntries = @($machinePath -split ';' | Where-Object { $_ })
    if ($pathEntries -notcontains $current) { [Environment]::SetEnvironmentVariable('Path', (($pathEntries + $current) -join ';'), 'Machine') }
    & (Join-Path $current 'rivo.exe') --version
} finally {
    Remove-Item -Recurse -Force -LiteralPath $temp -ErrorAction SilentlyContinue
}

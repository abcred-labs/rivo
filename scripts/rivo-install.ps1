[CmdletBinding()]
param(
    [Alias('h')]
    [switch]$Help,
    [string]$Version = $(if ($env:RIVO_INSTALL_VERSION) { $env:RIVO_INSTALL_VERSION } else { 'latest' }),
    [string]$InstallRoot = $(if ($env:RIVO_INSTALL_ROOT) { $env:RIVO_INSTALL_ROOT } else { Join-Path $env:ProgramFiles 'Rivo' }),
    [string]$BaseUrl = $(if ($env:RIVO_INSTALL_BASE_URL) { $env:RIVO_INSTALL_BASE_URL } else { '' }),
    [string]$GithubMirror = $(if ($env:RIVO_GITHUB_MIRROR) { $env:RIVO_GITHUB_MIRROR } else { '' }),
    [string]$ReleaseAssetsMirror = $(if ($env:RIVO_RELEASE_ASSETS_MIRROR) { $env:RIVO_RELEASE_ASSETS_MIRROR } else { '' })
)

$ErrorActionPreference = 'Stop'
if ($Help) {
    @'
Install the latest stable Rivo release.

Parameters:
  -Version VERSION      Install VERSION instead of latest
  -InstallRoot PATH     Override the system install root
  -BaseUrl URL           Use a release mirror
  -GithubMirror URL      Mirror for https://github.com (mainland China: https://github-mirror.abc.red)
  -ReleaseAssetsMirror URL
                         Mirror for https://release-assets.githubusercontent.com
                         (mainland China: https://gh-release-assets.abc.red)
  -Help                  Show this help

The mirror parameters can also be set with RIVO_GITHUB_MIRROR and
RIVO_RELEASE_ASSETS_MIRROR. Mirrors only speed up downloads; SHA-256 verification is unchanged.
'@
    exit 0
}
$repository = if ($env:RIVO_REPOSITORY) { $env:RIVO_REPOSITORY } else { 'abcred-labs/rivo' }
$githubOrigin = 'https://github.com'
$assetsOrigin = 'https://release-assets.githubusercontent.com'
$GithubMirror = $GithubMirror.Trim().TrimEnd('/')
$ReleaseAssetsMirror = $ReleaseAssetsMirror.Trim().TrimEnd('/')
foreach ($mirrorValue in @($GithubMirror, $ReleaseAssetsMirror)) {
    if ($mirrorValue -and $mirrorValue -notmatch '^https://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/-]*)?$') {
        throw 'RIVO_GITHUB_MIRROR and RIVO_RELEASE_ASSETS_MIRROR must be https:// URLs.'
    }
}
$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run this installer from an elevated PowerShell window.' }

$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($architecture -eq 'X64') { $arch = 'amd64' } elseif ($architecture -eq 'Arm64') { $arch = 'arm64' } else { throw "Unsupported Windows architecture: $architecture" }
if ($arch -ne 'amd64') { throw 'Windows release archives currently support amd64 only.' }

function Invoke-RivoDownload([string]$Url, [string]$OutFile) {
    # Follows redirects one HTTPS hop at a time so GitHub and release-asset
    # redirects can be rewritten to the configured mirrors.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    for ($hop = 0; $hop -lt 6; $hop++) {
        $request = [System.Net.HttpWebRequest]::Create($Url)
        $request.AllowAutoRedirect = $false
        $request.UserAgent = 'rivo-installer'
        try {
            $response = $request.GetResponse()
        } catch [System.Net.WebException] {
            if ($null -eq $_.Exception.Response) { throw }
            $response = $_.Exception.Response
        }
        try {
            $status = [int]$response.StatusCode
            if ($status -ge 300 -and $status -lt 400) {
                $location = $response.Headers['Location']
                if ([string]::IsNullOrWhiteSpace($location)) { throw "HTTP $status from $Url has no Location header." }
                $next = (New-Object System.Uri((New-Object System.Uri($Url)), $location)).AbsoluteUri
                if (-not $next.StartsWith('https://')) { throw 'Refusing non-HTTPS redirect.' }
                if ($GithubMirror -and $next.StartsWith("$githubOrigin/")) { $next = $GithubMirror + $next.Substring($githubOrigin.Length) }
                if ($ReleaseAssetsMirror -and $next.StartsWith("$assetsOrigin/")) { $next = $ReleaseAssetsMirror + $next.Substring($assetsOrigin.Length) }
                $Url = $next
                continue
            }
            if ($status -ne 200) { throw "HTTP $status from $Url." }
            $source = $response.GetResponseStream()
            $target = [IO.File]::Create($OutFile)
            try { $source.CopyTo($target) } finally { $target.Dispose(); $source.Dispose() }
            return
        } finally {
            $response.Dispose()
        }
    }
    throw "Too many redirects from $Url."
}

if ([string]::IsNullOrWhiteSpace($BaseUrl)) {
    $downloadOrigin = if ($GithubMirror) { $GithubMirror } else { $githubOrigin }
    if ($Version -eq 'latest') { $BaseUrl = "$downloadOrigin/$repository/releases/latest/download" } else { $BaseUrl = "$downloadOrigin/$repository/releases/download/v$Version" }
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
    Invoke-RivoDownload $manifestUrl $manifestPath
    $manifest = Get-Content -Raw -LiteralPath $manifestPath | ConvertFrom-Json
    if ($manifest.schema -ne 2 -or $manifest.product -ne 'rivo') { throw 'Unsupported Rivo release manifest.' }
    if ($manifest.channel -ne 'stable') { throw 'Release manifest is not a stable release.' }
    if ($manifest.platform -ne 'windows' -or $manifest.arch -ne 'amd64') { throw 'Release manifest target mismatch.' }
    if ($manifest.version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw 'Invalid release version.' }
    if ($Version -ne 'latest' -and $Version -ne $manifest.version) { throw "Requested version $Version does not match manifest $($manifest.version)." }
    $artifacts = @($manifest.artifacts)
    if ($artifacts.Count -ne 1 -or $null -eq $artifacts[0]) { throw 'Release manifest must contain exactly one artifact.' }
    $artifact = $artifacts[0]
    $expectedName = "rivo-$($manifest.version)-windows-amd64.zip"
    if ([string]::IsNullOrWhiteSpace([string]$artifact.file) -or $artifact.file -ne $expectedName) { throw 'Release artifact does not match the target.' }
    $manifestSha = [string]$artifact.sha256
    if ($manifestSha -notmatch '^[0-9a-fA-F]{64}$') { throw 'Release manifest contains an invalid artifact checksum.' }
    $artifactBase = if ($mirror) { "$BaseUrl/windows-amd64" } else { $BaseUrl }
    $archive = Join-Path $temp $artifact.file
    $sidecar = "$archive.sha256"
    Invoke-RivoDownload "$artifactBase/$($artifact.file)" $archive
    Invoke-RivoDownload "$artifactBase/$($artifact.file).sha256" $sidecar
    $hash = Get-FileHash -Algorithm SHA256 -LiteralPath $archive
    if ($null -eq $hash -or [string]::IsNullOrWhiteSpace([string]$hash.Hash)) { throw 'Unable to calculate the release checksum.' }
    $actual = ([string]$hash.Hash).ToLowerInvariant()
    $sidecarText = Get-Content -Raw -LiteralPath $sidecar
    if ($null -eq $sidecarText -or [string]::IsNullOrWhiteSpace($sidecarText)) { throw 'Release checksum sidecar is empty.' }
    $sidecarFields = $sidecarText.Trim() -split '\s+'
    if ($sidecarFields.Count -lt 1 -or $sidecarFields[0] -notmatch '^[0-9a-fA-F]{64}$') { throw 'Release checksum sidecar is invalid.' }
    $declared = ([string]$sidecarFields[0]).ToLowerInvariant()
    if ($actual -ne $manifestSha.ToLowerInvariant() -or $actual -ne $declared) { throw 'Release checksum verification failed.' }

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
    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $pathEntries = @($machinePath -split ';' | Where-Object { $_ })
    if ($pathEntries -notcontains $current) { [Environment]::SetEnvironmentVariable('Path', (($pathEntries + $current) -join ';'), 'Machine') }
    & (Join-Path $current 'rivo.exe') --version
} finally {
    Remove-Item -Recurse -Force -LiteralPath $temp -ErrorAction SilentlyContinue
}

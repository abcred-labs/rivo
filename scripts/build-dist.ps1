<#
.SYNOPSIS
  Build the rivo CLI binary and copy it into the repo-root dist/ directory for manual testing.
.PARAMETER Configuration
  'Release' (default) or 'Debug'.
.EXAMPLE
  ./scripts/build-dist.ps1
  ./scripts/build-dist.ps1 -Configuration Debug
#>
param(
  [ValidateSet('Release', 'Debug')]
  [string]$Configuration = 'Release'
)
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$runtimeDir = Join-Path $repoRoot 'runtime'
$distDir = Join-Path $repoRoot 'dist'

Push-Location $runtimeDir
try {
  if ($Configuration -eq 'Release') {
    cargo build --release -p rivo --locked
  } else {
    cargo build -p rivo --locked
  }
  if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
} finally {
  Pop-Location
}

$targetSubdir = if ($Configuration -eq 'Release') { 'release' } else { 'debug' }
$builtBinary = Join-Path $runtimeDir "target\$targetSubdir\rivo.exe"
if (!(Test-Path -LiteralPath $builtBinary)) { throw "binary not found: $builtBinary" }

& (Join-Path $PSScriptRoot 'verify-windows-metadata.ps1') -Binary $builtBinary

New-Item -ItemType Directory -Force -Path $distDir | Out-Null
Copy-Item -LiteralPath $builtBinary -Destination (Join-Path $distDir 'rivo.exe') -Force
$languageSource = Join-Path $runtimeDir 'languages'
$languageTarget = Join-Path $distDir 'share\rivo\languages'
New-Item -ItemType Directory -Force -Path $languageTarget | Out-Null
foreach ($locale in @('en-US.ftl', 'zh-CN.ftl')) {
  $source = Join-Path $languageSource $locale
  if (!(Test-Path -LiteralPath $source)) { throw "bundled language file missing: $source" }
  Copy-Item -LiteralPath $source -Destination (Join-Path $languageTarget $locale) -Force
}

Write-Host "Built ($Configuration): $(Join-Path $distDir 'rivo.exe')"

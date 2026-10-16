<#
.SYNOPSIS
  Verify the Windows version metadata embedded in a Rivo executable.
.PARAMETER Binary
  Path to the executable to inspect.
.PARAMETER ExpectedVersion
  Optional Cargo version. When omitted, it is read from runtime/Cargo.toml.
#>
param(
  [Parameter(Mandatory = $true)][string]$Binary,
  [string]$ExpectedVersion
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
  throw "binary not found: $Binary"
}

$repoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($ExpectedVersion)) {
  $manifest = Get-Content -Raw -LiteralPath (Join-Path $repoRoot 'runtime/Cargo.toml')
  $match = [regex]::Match($manifest, '(?ms)^\[workspace\.package\].*?^version\s*=\s*"([^"]+)"')
  if (-not $match.Success) {
    throw 'workspace package version missing from runtime/Cargo.toml'
  }
  $ExpectedVersion = $match.Groups[1].Value
}

$info = [System.Diagnostics.FileVersionInfo]::GetVersionInfo((Resolve-Path -LiteralPath $Binary))
$expected = [ordered]@{
  FileDescription = 'Rivo Agent CLI'
  FileVersion = $ExpectedVersion
  ProductName = 'Rivo'
  ProductVersion = $ExpectedVersion
  CompanyName = 'Rivo'
  LegalCopyright = 'Copyright (c) 2026 Rivo contributors'
  InternalName = 'rivo'
  OriginalFilename = 'rivo.exe'
  Language = 'English (United States)'
}

$actual = [ordered]@{
  FileDescription = $info.FileDescription
  FileVersion = $info.FileVersion
  ProductName = $info.ProductName
  ProductVersion = $info.ProductVersion
  CompanyName = $info.CompanyName
  LegalCopyright = $info.LegalCopyright
  InternalName = $info.InternalName
  OriginalFilename = $info.OriginalFilename
  Language = $info.Language
}

$mismatches = @(
  foreach ($name in $expected.Keys) {
    if ($actual[$name] -ne $expected[$name]) {
      "${name}: expected '$($expected[$name])', got '$($actual[$name])'"
    }
  }
)

if ($mismatches.Count -gt 0) {
  $details = $mismatches -join [Environment]::NewLine
  throw "Windows version metadata verification failed for $Binary$([Environment]::NewLine)$details"
}

Write-Host "Windows version metadata verified: $Binary ($ExpectedVersion)"

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][ValidateSet('chrome','edge')][string]$Browser,
  [string]$ExtensionId = 'adnanfbbgelpelombhdjflmcggepbcee',
  [string]$RivoExecutable = '',
  [string]$HostExecutable = '',
  [string]$BrowserExecutable = '',
  [string]$OutputDirectory = '',
  [switch]$Build
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($RivoExecutable)) { $RivoExecutable = Join-Path $repoRoot 'runtime\target\debug\rivo.exe' }
if ([string]::IsNullOrWhiteSpace($HostExecutable)) { $HostExecutable = Join-Path $repoRoot 'runtime\target\debug\rivo-browser-extension-host.exe' }
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) { $OutputDirectory = Join-Path $repoRoot 'artifacts\test-reports\20261010-chineselearning-seo' }
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$extensionRoot = Join-Path $repoRoot 'packaging\browser-extension'
$profileRoot = Join-Path ([IO.Path]::GetTempPath()) ('rivo-seo-' + [guid]::NewGuid().ToString('N'))
$pipeName = 'rivo-browser-extension-seo-' + [guid]::NewGuid().ToString('N')
$pipePath = "\\.\pipe\$pipeName"
$browserProcess = $null
$oldPipe = $env:RIVO_BROWSER_EXTENSION_PIPE
$registryPath = if ($Browser -eq 'chrome') { 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.rivo.browser' } else { 'HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.rivo.browser' }
$manifestPath = Join-Path $env:LOCALAPPDATA "Rivo\NativeMessagingHosts\com.rivo.browser.$Browser.json"
$hadRegistryKey = Test-Path -LiteralPath $registryPath
$oldManifestPath = if ($hadRegistryKey) { (Get-ItemProperty -LiteralPath $registryPath -Name '(Default)' -ErrorAction SilentlyContinue).'(Default)' } else { $null }
$hadManifest = Test-Path -LiteralPath $manifestPath
$oldManifest = if ($hadManifest) { [IO.File]::ReadAllText($manifestPath) } else { $null }

function Invoke-SeoSmoke([string]$Url, [string]$JsonName, [string]$PngName) {
  $rivoArgs = @(
    'browser-extension-seo-smoke', '--family', $Browser, '--pipe', $pipePath,
    '--url', $Url,
    '--output', (Join-Path $OutputDirectory $JsonName),
    '--screenshot', (Join-Path $OutputDirectory $PngName)
  )
  $output = & $RivoExecutable @rivoArgs 2>&1
  $exitCode = $LASTEXITCODE
  $output | Write-Host
  return $exitCode
}

try {
  if ($Build) {
    Push-Location (Join-Path $repoRoot 'runtime')
    cargo build -p rivo --bin rivo --bin rivo-browser-extension-host --locked
    if ($LASTEXITCODE -ne 0) { throw 'Rivo build failed' }
    Pop-Location
  }
  $RivoExecutable = (Resolve-Path -LiteralPath $RivoExecutable -ErrorAction Stop).Path
  $HostExecutable = (Resolve-Path -LiteralPath $HostExecutable -ErrorAction Stop).Path
  if (-not (Test-Path -LiteralPath $extensionRoot)) { throw 'Browser extension package not found' }
  if ($ExtensionId -notmatch '^[a-p]{32}$') { throw 'Invalid extension ID' }
  if (Test-Path -LiteralPath $OutputDirectory) {
    $existing = @(Get-ChildItem -LiteralPath $OutputDirectory -Force)
    if ($existing.Count -gt 0) { throw "Output directory must be empty: $OutputDirectory" }
  } else {
    New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
  }

  if ([string]::IsNullOrWhiteSpace($BrowserExecutable)) {
    $browserCandidates = if ($Browser -eq 'chrome') {
      @("$env:LOCALAPPDATA\ms-playwright\chromium-1200\chrome-win64\chrome.exe", "$env:ProgramFiles\Google\Chrome\Application\chrome.exe", "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe")
    } else {
      @("$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe", "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe", "$env:LOCALAPPDATA\Microsoft\Edge\Application\msedge.exe")
    }
    $browserPath = $browserCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
  } else {
    $browserPath = (Resolve-Path -LiteralPath $BrowserExecutable -ErrorAction Stop).Path
  }
  if (-not $browserPath) { throw "$Browser executable not found" }

  & (Join-Path $extensionRoot 'install-native-host.ps1') -Browser $Browser -ExtensionId $ExtensionId -HostExecutable $HostExecutable -PipeName $pipeName | Write-Host
  $env:RIVO_BROWSER_EXTENSION_PIPE = $pipePath
  New-Item -ItemType Directory -Force -Path $profileRoot | Out-Null
  $browserArgLine = '--user-data-dir="' + $profileRoot + '" --no-first-run --no-default-browser-check --no-sandbox --enable-automation --test-type --load-extension="' + $extensionRoot + '" about:blank'
  $browserLog = Join-Path $profileRoot 'browser-stderr.log'
  $browserProcess = Start-Process -FilePath $browserPath -ArgumentList $browserArgLine -RedirectStandardError $browserLog -PassThru
  Start-Sleep -Seconds 4
  if ($browserProcess.HasExited) { throw "Browser exited early with code $($browserProcess.ExitCode)" }
  if ((Test-Path -LiteralPath $browserLog) -and (Select-String -LiteralPath $browserLog -SimpleMatch '--load-extension is not allowed' -Quiet)) {
    throw 'Browser refused command-line extension loading; provide a Chrome for Testing/Chromium executable or Edge.'
  }

  $googleUrl = 'https://www.google.com/search?q=chineselearning.com%20SEO'
  $googleExit = Invoke-SeoSmoke $googleUrl 'google-result.json' 'browser-google-search.png'
  $targetExit = Invoke-SeoSmoke 'https://chineselearning.com' 'chineselearning-seo.json' 'chineselearning-seo-page.png'
  Write-Host "browser-extension-seo-smoke: google_exit=$googleExit target_exit=$targetExit output=$OutputDirectory"
  if ($targetExit -ne 0) { throw "Rivo target-site smoke failed with exit code $targetExit" }
}
finally {
  if ($browserProcess) {
    & taskkill.exe /PID $browserProcess.Id /T /F 2>$null | Out-Null
    if (-not $browserProcess.HasExited) { [void]$browserProcess.WaitForExit(5000) }
  }
  if ($null -eq $oldPipe) { Remove-Item Env:RIVO_BROWSER_EXTENSION_PIPE -ErrorAction SilentlyContinue }
  else { $env:RIVO_BROWSER_EXTENSION_PIPE = $oldPipe }

  if ($hadRegistryKey) {
    if ($oldManifestPath) { Set-ItemProperty -LiteralPath $registryPath -Name '(Default)' -Value $oldManifestPath }
  } else {
    Remove-Item -LiteralPath $registryPath -Force -ErrorAction SilentlyContinue
  }
  if ($hadManifest) {
    [IO.File]::WriteAllText($manifestPath, $oldManifest, [Text.UTF8Encoding]::new($false))
  } else {
    Remove-Item -LiteralPath $manifestPath -Force -ErrorAction SilentlyContinue
  }
  if (Test-Path -LiteralPath $profileRoot) { Remove-Item -LiteralPath $profileRoot -Recurse -Force -ErrorAction SilentlyContinue }
}

[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][ValidateSet('chrome','edge')][string]$Browser,
  [ValidatePattern('^[a-p]{32}$')][string]$ExtensionId = 'adnanfbbgelpelombhdjflmcggepbcee',
  [string]$RivoExecutable = '',
  [string]$HostExecutable = '',
  [string]$BrowserExecutable = '',
  [switch]$Build,
  [switch]$KeepProfile,
  [switch]$TestReconnect,
  [int]$Port = 18765
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$ifDefaultRivo = Join-Path $repoRoot 'runtime\target\debug\rivo.exe'
$ifDefaultHost = Join-Path $repoRoot 'runtime\target\debug\rivo-browser-extension-host.exe'
if ([string]::IsNullOrWhiteSpace($RivoExecutable)) { $RivoExecutable = $ifDefaultRivo }
if ([string]::IsNullOrWhiteSpace($HostExecutable)) { $HostExecutable = $ifDefaultHost }
$extensionRoot = Join-Path $repoRoot 'packaging\browser-extension'
$pipeName = "\\.\pipe\rivo-browser-extension-smoke-$Browser-$([guid]::NewGuid().ToString('N'))"
$profileRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("rivo-browser-extension-$Browser-" + [guid]::NewGuid().ToString('N'))
$serverRoot = Join-Path $profileRoot 'site'
$browserProcess = $null
$serverProcess = $null
$oldPipe = $env:RIVO_BROWSER_EXTENSION_PIPE
$registryPath = if ($Browser -eq 'chrome') { 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.rivo.browser' } else { 'HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.rivo.browser' }
$managedManifest = Join-Path $env:LOCALAPPDATA "Rivo\NativeMessagingHosts\com.rivo.browser.$Browser.json"
$hadRegistry = Test-Path -LiteralPath $registryPath
$oldRegistryManifest = $null
$manifestBackups = @{}
if ($hadRegistry) {
  $registryKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($registryPath.Substring(6))
  try { $oldRegistryManifest = $registryKey.GetValue('') } finally { $registryKey.Dispose() }
}
foreach ($manifestPath in @($oldRegistryManifest, $managedManifest) | Where-Object { $_ }) {
  if ((Test-Path -LiteralPath $manifestPath) -and -not $manifestBackups.ContainsKey($manifestPath)) {
    $manifestBackups[$manifestPath] = [IO.File]::ReadAllBytes($manifestPath)
  }
}

function Fail([string]$Message) { throw "Browser extension smoke ($Browser): $Message" }

function Get-DescendantProcessIds([int]$RootProcessId) {
  $allProcesses = @(Get-CimInstance Win32_Process)
  $ids = [Collections.Generic.HashSet[int]]::new()
  [void]$ids.Add($RootProcessId)
  do {
    $changed = $false
    foreach ($process in $allProcesses) {
      if ($ids.Contains([int]$process.ParentProcessId) -and $ids.Add([int]$process.ProcessId)) { $changed = $true }
    }
  } while ($changed)
  return @($ids)
}

function Stop-BrowserTree($Process) {
  if ($Process) {
    & taskkill.exe /PID $Process.Id /T /F 2>$null | Out-Null
    if (-not $Process.HasExited) { [void]$Process.WaitForExit(5000) }
  }
}

try {
  if ($Build) {
    Push-Location (Join-Path $repoRoot 'runtime')
    cargo build -p rivo --bin rivo --bin rivo-browser-extension-host --locked
    if ($LASTEXITCODE -ne 0) { Fail 'cargo build failed' }
    Pop-Location
  }
  $rivo = (Resolve-Path $RivoExecutable -ErrorAction Stop).Path
  $hostBinary = (Resolve-Path $HostExecutable -ErrorAction Stop).Path
  if (-not (Test-Path $extensionRoot)) { Fail "extension directory not found: $extensionRoot" }

  New-Item -ItemType Directory -Force -Path $serverRoot | Out-Null
  Copy-Item -LiteralPath (Join-Path $repoRoot 'scripts\fixtures\browser-extension-smoke.html') -Destination (Join-Path $serverRoot 'index.html')

  $python = Get-Command python.exe -ErrorAction SilentlyContinue
  if ($null -eq $python) { Fail 'python.exe is required for the local smoke page' }
  $serverProcess = Start-Process -FilePath $python.Source -ArgumentList @('-m','http.server', $Port, '--bind','127.0.0.1') -WorkingDirectory $serverRoot -PassThru -WindowStyle Hidden
  & (Join-Path $extensionRoot 'install-native-host.ps1') -Browser $Browser -ExtensionId $ExtensionId -HostExecutable $hostBinary | Write-Host

  if ([string]::IsNullOrWhiteSpace($BrowserExecutable)) {
    $browserCandidates = if ($Browser -eq 'chrome') {
      @("$env:ProgramFiles\Google\Chrome\Application\chrome.exe", "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe")
    } else {
      @("$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe", "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe", "$env:LOCALAPPDATA\Microsoft\Edge\Application\msedge.exe")
    }
    $browserPath = $browserCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
  } else {
    $browserPath = (Resolve-Path $BrowserExecutable -ErrorAction Stop).Path
  }
  if (-not $browserPath) { Fail "$Browser executable not found" }

  $env:RIVO_BROWSER_EXTENSION_PIPE = $pipeName
  New-Item -ItemType Directory -Force -Path $profileRoot | Out-Null
  $browserArgLine = '--user-data-dir="' + $profileRoot + '" --no-first-run --no-default-browser-check --no-sandbox --enable-automation --test-type --load-extension="' + $extensionRoot + '" --enable-logging=stderr --v=1 http://127.0.0.1:' + $Port + '/index.html'
  $browserLog = Join-Path $profileRoot 'browser-stderr.log'
  $browserProcess = Start-Process -FilePath $browserPath -ArgumentList $browserArgLine -RedirectStandardError $browserLog -PassThru -WindowStyle Hidden
  Start-Sleep -Seconds 3
  if ($browserProcess.HasExited) {
    $log = if (Test-Path $browserLog) { Get-Content $browserLog -Raw } else { '' }
    Fail "browser exited early with code $($browserProcess.ExitCode): $log"
  }
  if ((Test-Path $browserLog) -and (Select-String -Path $browserLog -SimpleMatch '--load-extension is not allowed' -Quiet)) {
    Fail "$Browser branded build rejects command-line unpacked extension loading; load the Rivo extension manually in a developer profile or use Chromium/Chrome for Testing"
  }
  if ($TestReconnect) {
    $browserTree = Get-DescendantProcessIds $browserProcess.Id
    $hostProcesses = @(Get-CimInstance Win32_Process | Where-Object {
      $_.ProcessId -in $browserTree -and $_.ExecutablePath -and
      [string]::Equals($_.ExecutablePath, $hostBinary, [StringComparison]::OrdinalIgnoreCase)
    })
    if ($hostProcesses.Count -eq 0) { Fail 'could not identify this smoke Profile Native Messaging host process' }
    foreach ($hostProcess in $hostProcesses) { Stop-Process -Id $hostProcess.ProcessId -Force -ErrorAction Stop }
    Start-Sleep -Seconds 3
  }

  $output = & $rivo browser-extension-smoke --family $Browser --pipe $pipeName --url "http://127.0.0.1:$Port/index.html" 2>&1
  $exitCode = $LASTEXITCODE
  $output | Write-Host
  if ($exitCode -ne 0) { Fail "Rivo smoke command failed with exit code $exitCode" }
  Write-Host "browser-extension-smoke: browser=$Browser status=passed"
}
finally {
  Stop-BrowserTree $browserProcess
  if ($serverProcess -and -not $serverProcess.HasExited) { Stop-Process -Id $serverProcess.Id -Force -ErrorAction SilentlyContinue }
  if ($hadRegistry) {
    New-Item -Force -Path $registryPath | Out-Null
    Set-ItemProperty -LiteralPath $registryPath -Name '(Default)' -Value $oldRegistryManifest
  } elseif (Test-Path -LiteralPath $registryPath) {
    Remove-Item -LiteralPath $registryPath -Force
  }
  foreach ($manifestPath in $manifestBackups.Keys) { [IO.File]::WriteAllBytes($manifestPath, $manifestBackups[$manifestPath]) }
  if (-not $manifestBackups.ContainsKey($managedManifest) -and (Test-Path -LiteralPath $managedManifest)) {
    Remove-Item -LiteralPath $managedManifest -Force
  }
  if ($null -eq $oldPipe) { Remove-Item Env:RIVO_BROWSER_EXTENSION_PIPE -ErrorAction SilentlyContinue }
  else { $env:RIVO_BROWSER_EXTENSION_PIPE = $oldPipe }
  if ($KeepProfile) { Write-Host "browser-extension-smoke: kept profile $profileRoot" }
  elseif (Test-Path $profileRoot) { Remove-Item -LiteralPath $profileRoot -Recurse -Force -ErrorAction SilentlyContinue }
}

[CmdletBinding()]
param(
  [ValidateSet('chrome','edge')][string]$Browser = 'chrome',
  [string]$BrowserExecutable = '',
  [string]$RivoExecutable = '',
  [string]$HostExecutable = '',
  [ValidatePattern('^[a-p]{32}$')][string]$ExtensionId = 'adnanfbbgelpelombhdjflmcggepbcee',
  [int]$Port = 18766,
  [switch]$KeepProfiles
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $RivoExecutable) { $RivoExecutable = Join-Path $repoRoot 'runtime\target\debug\rivo.exe' }
if (-not $HostExecutable) { $HostExecutable = Join-Path $repoRoot 'runtime\target\debug\rivo-browser-extension-host.exe' }
$rivo = (Resolve-Path -LiteralPath $RivoExecutable -ErrorAction Stop).Path
$hostBinary = (Resolve-Path -LiteralPath $HostExecutable -ErrorAction Stop).Path
$extensionRoot = Join-Path $repoRoot 'packaging\browser-extension'
$fixturePath = Join-Path $repoRoot 'scripts\fixtures\browser-extension-smoke.html'
$pipeName = "rivo-browser-extension-profiles-$([guid]::NewGuid().ToString('N'))"
$pipePath = "\\.\pipe\$pipeName"
$profileRoot = Join-Path ([IO.Path]::GetTempPath()) ("rivo-browser-extension-profiles-" + [guid]::NewGuid().ToString('N'))
$profileA = Join-Path $profileRoot 'profile-a'
$profileB = Join-Path $profileRoot 'profile-b'
$serverRoot = Join-Path $profileRoot 'site'
$browserA = $null
$browserB = $null
$server = $null
$heldConnections = @()
$oldPipe = $env:RIVO_BROWSER_EXTENSION_PIPE
$registryPath = if ($Browser -eq 'chrome') { 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.rivo.browser' } else { 'HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.rivo.browser' }
$managedManifest = Join-Path $env:LOCALAPPDATA "Rivo\NativeMessagingHosts\com.rivo.browser.$Browser.json"
$hadRegistry = Test-Path -LiteralPath $registryPath
$oldRegistryManifest = $null
$manifestBackups = @{}
$fallbackRegistryBackups = @{}

# Edge also searches Chromium/Chrome Native Messaging registrations. Leaving
# those present would let Profile A connect to an unrelated older Host before
# this test registers its own executable.
if ($Browser -eq 'edge') {
  foreach ($fallbackPath in @('HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.rivo.browser', 'HKCU:\Software\Chromium\NativeMessagingHosts\com.rivo.browser')) {
    if (Test-Path -LiteralPath $fallbackPath) {
      $fallbackKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($fallbackPath.Substring(6))
      try { $fallbackRegistryBackups[$fallbackPath] = $fallbackKey.GetValue('') } finally { $fallbackKey.Dispose() }
    }
  }
}

if ($hadRegistry) {
  $registryKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($registryPath.Substring(6))
  try { $oldRegistryManifest = $registryKey.GetValue('') } finally { $registryKey.Dispose() }
}
foreach ($manifestPath in @($oldRegistryManifest, $managedManifest) | Where-Object { $_ }) {
  if ((Test-Path -LiteralPath $manifestPath) -and -not $manifestBackups.ContainsKey($manifestPath)) {
    $manifestBackups[$manifestPath] = [IO.File]::ReadAllBytes($manifestPath)
  }
}

function Read-Exact([IO.Stream]$Stream, [byte[]]$Buffer) {
  $offset = 0
  while ($offset -lt $Buffer.Length) {
    $readTask = $Stream.ReadAsync($Buffer, $offset, $Buffer.Length - $offset, [Threading.CancellationToken]::None)
    if (-not $readTask.Wait(3000)) { throw 'named-pipe frame read timed out' }
    $read = $readTask.GetAwaiter().GetResult()
    if ($read -le 0) { throw 'short named-pipe frame' }
    $offset += $read
  }
}

function Read-Frame([IO.Stream]$Stream) {
  $header = New-Object byte[] 4
  Read-Exact $Stream $header
  $length = [BitConverter]::ToUInt32($header, 0)
  if ($length -gt 1048576) { throw 'named-pipe message exceeds 1 MiB' }
  $body = New-Object byte[] $length
  Read-Exact $Stream $body
  [Text.Encoding]::UTF8.GetString($body) | ConvertFrom-Json
}

function Encode-Frame($Value) {
  $json = $Value | ConvertTo-Json -Compress -Depth 12
  $body = [Text.Encoding]::UTF8.GetBytes($json)
  $frame = New-Object byte[] (4 + $body.Length)
  [BitConverter]::GetBytes([uint32]$body.Length).CopyTo($frame, 0)
  [Array]::Copy($body, 0, $frame, 4, $body.Length)
  $frame
}

function Get-Tabs($Connection) {
  $hello = $Connection.Hello
  $instance = [ordered]@{
    browser_family = $hello.browser_family
    extension_id = $hello.extension_id
    installation_id = $hello.installation_id
    profile_key = $hello.profile_key
    boot_id = $hello.boot_id
  }
  $requestId = "profile-probe-$([guid]::NewGuid().ToString('N'))"
  $request = [ordered]@{
    protocol = 'rivo-browser-extension-v1'
    id = $requestId
    method = 'browser_list_tabs'
    browser_instance = $instance
    params = @{}
    deadline_ms = 5000
  }
  $frame = Encode-Frame $request
  $Connection.Stream.Write($frame, 0, $frame.Length)
  $Connection.Stream.Flush()
  $response = Read-Frame $Connection.Stream
  if (-not $response.ok -or $response.id -ne $requestId -or $response.browser_instance.boot_id -ne $hello.boot_id) {
    throw 'profile tab probe failed request/response identity validation'
  }
  return @($response.result)
}

function Get-InstanceHandshakes([int]$Count) {
  $connections = [Collections.Generic.List[object]]::new()
  for ($attempt = 0; $attempt -lt 100 -and $connections.Count -lt $Count; $attempt++) {
    $pipe = [IO.Pipes.NamedPipeClientStream]::new('.', $pipeName, [IO.Pipes.PipeDirection]::InOut)
    try {
      $pipe.Connect(300)
      $hello = Read-Frame $pipe
      if ($hello.protocol -ne 'rivo-browser-extension-v1' -or $hello.browser_family -ne $Browser -or $hello.extension_id -ne $ExtensionId) {
        throw 'browser profile returned an unexpected extension identity'
      }
      if (@($connections | Where-Object { $_.Hello.installation_id -eq $hello.installation_id }).Count -eq 0) {
        $connections.Add([pscustomobject]@{ Stream = $pipe; Hello = $hello })
        $pipe = $null
      }
    } catch {
      if ($attempt -eq 99) { throw }
    } finally {
      if ($pipe) { $pipe.Dispose() }
    }
    if ($connections.Count -lt $Count) { Start-Sleep -Milliseconds 200 }
  }
  if ($connections.Count -ne $Count) {
    foreach ($connection in $connections) { $connection.Stream.Dispose() }
    throw "expected $Count distinct Native Messaging profiles; connected $($connections.Count)"
  }
  return ,$connections.ToArray()
}

function Close-Handshakes($Connections) {
  foreach ($connection in $Connections) { $connection.Stream.Dispose() }
}

function Start-SmokeBrowser([string]$DataDir, [string]$ProfileName, [string]$LogPath) {
  New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
  $url = "http://127.0.0.1:$Port/index.html?profile=$ProfileName"
  $argLine = '--user-data-dir="' + $DataDir + '" --no-first-run --no-default-browser-check --no-sandbox --enable-automation --test-type --load-extension="' + $extensionRoot + '" --window-size=1200,900 http://127.0.0.1:' + $Port + '/index.html?profile=' + $ProfileName
  $process = Start-Process -FilePath $BrowserExecutable -ArgumentList $argLine -RedirectStandardError $LogPath -PassThru -WindowStyle Hidden
  Start-Sleep -Seconds 2
  if ($process.HasExited) {
    $log = if (Test-Path -LiteralPath $LogPath) { Get-Content -LiteralPath $LogPath -Raw } else { '' }
    throw "$Browser exited before smoke startup (code $($process.ExitCode)): $log"
  }
  if ((Test-Path -LiteralPath $LogPath) -and (Select-String -Path $LogPath -SimpleMatch '--load-extension is not allowed' -Quiet)) {
    throw "selected $Browser build rejects command-line unpacked extension loading"
  }
  $profileArgument = '--user-data-dir="' + $DataDir + '"'
  $profileRootProcess = Get-CimInstance Win32_Process | Where-Object {
    $_.Name -eq [IO.Path]::GetFileName($BrowserExecutable) -and
    $_.CommandLine -and $_.CommandLine.Contains($profileArgument) -and
    $_.CommandLine -notmatch '--type='
  } | Select-Object -First 1
  if (-not $profileRootProcess) { throw "$Browser did not start an isolated process for Profile $ProfileName" }
  [pscustomobject]@{ Process = (Get-Process -Id $profileRootProcess.ProcessId); Url = $url; DataDir = $DataDir }
}

function Get-SmokeBrowserProcesses($Browser) {
  if (-not $Browser) { return @() }
  $escapedDataDir = [regex]::Escape($Browser.DataDir)
  return @(Get-CimInstance Win32_Process | Where-Object {
    $_.Name -eq [IO.Path]::GetFileName($BrowserExecutable) -and
    $_.CommandLine -and $_.CommandLine -match $escapedDataDir
  })
}

function Assert-ProfileHost($Profile) {
  $processes = @(Get-CimInstance Win32_Process)
  $ids = [Collections.Generic.HashSet[int]]::new()
  [void]$ids.Add($Profile.Process.Id)
  do {
    $changed = $false
    foreach ($process in $processes) {
      if ($ids.Contains([int]$process.ParentProcessId) -and $ids.Add([int]$process.ProcessId)) { $changed = $true }
    }
  } while ($changed)
  $hosts = @($processes | Where-Object { $ids.Contains([int]$_.ProcessId) -and $_.Name -eq [IO.Path]::GetFileName($hostBinary) })
  if ($hosts.Count -ne 1 -or -not [string]::Equals($hosts[0].ExecutablePath, $hostBinary, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Profile did not connect to the selected Native Messaging Host executable'
  }
}

function Stop-SmokeBrowser($Browser) {
  if (-not $Browser) { return }
  if (-not $Browser.Process.HasExited) {
    try { [void]$Browser.Process.CloseMainWindow() } catch {}
    [void]$Browser.Process.WaitForExit(10000)
  }
  $remaining = Get-SmokeBrowserProcesses $Browser
  if ($remaining.Count -gt 0) {
    $root = $remaining | Where-Object { $_.CommandLine -notmatch '--type=' } | Select-Object -First 1
    if ($root) { & taskkill.exe /PID $root.ProcessId /T /F 2>$null | Out-Null }
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
      Start-Sleep -Milliseconds 200
      $remaining = Get-SmokeBrowserProcesses $Browser
    } while ($remaining.Count -gt 0 -and [DateTime]::UtcNow -lt $deadline)
  }
  return ($remaining.Count -eq 0)
}

function Invoke-ProfileSmoke($Hello, [string]$ProfileName, [string]$Url) {
  $rivoArgs = @(
    'browser-extension-smoke', '--family', $Browser, '--pipe', $pipePath,
    '--url', $Url,
    '--extension-id', $Hello.extension_id,
    '--installation-id', $Hello.installation_id,
    '--profile-key', $Hello.profile_key,
    '--boot-id', $Hello.boot_id,
    '--expect-marker', "Rivo profile: $ProfileName"
  )
  $output = & $rivo @rivoArgs 2>&1
  $code = $LASTEXITCODE
  $output | Write-Host
  if ($code -ne 0) { throw "Profile $ProfileName browser action smoke failed ($code)" }
  if (($output -join "`n") -notmatch [regex]::Escape($Hello.boot_id)) { throw "Profile $ProfileName returned an unexpected instance identity" }
}

if (-not $BrowserExecutable) {
  $candidates = if ($Browser -eq 'chrome') {
    @(
      "$env:LOCALAPPDATA\Temp\rivo-chrome-for-testing\extracted\chrome-win64\chrome.exe",
      "$env:LOCALAPPDATA\ms-playwright\chromium-1228\chrome-win64\chrome.exe",
      "$env:ProgramFiles\Google\Chrome\Application\chrome.exe"
    )
  } else {
    @("$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe", "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe", "$env:LOCALAPPDATA\Microsoft\Edge\Application\msedge.exe")
  }
  $BrowserExecutable = $candidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
}
if (-not $BrowserExecutable) { throw "$Browser executable not found" }
$BrowserExecutable = (Resolve-Path -LiteralPath $BrowserExecutable -ErrorAction Stop).Path
$extensionRoot = (Resolve-Path -LiteralPath $extensionRoot).Path
if (-not (Test-Path -LiteralPath $fixturePath)) { throw "smoke fixture not found: $fixturePath" }
$python = Get-Command python.exe -ErrorAction SilentlyContinue
if (-not $python) { throw 'python.exe is required for the local smoke page' }

try {
  New-Item -ItemType Directory -Force -Path $profileRoot, $serverRoot | Out-Null
  Copy-Item -LiteralPath $fixturePath -Destination (Join-Path $serverRoot 'index.html')
  $server = Start-Process -FilePath $python.Source -ArgumentList @('-m','http.server',$Port,'--bind','127.0.0.1') -WorkingDirectory $serverRoot -PassThru -WindowStyle Hidden
  $env:RIVO_BROWSER_EXTENSION_PIPE = $pipePath

  if (Test-Path -LiteralPath $registryPath) { Remove-Item -LiteralPath $registryPath -Force }
  foreach ($fallbackPath in $fallbackRegistryBackups.Keys) { Remove-Item -LiteralPath $fallbackPath -Force }
  $browserA = Start-SmokeBrowser $profileA 'A' (Join-Path $profileRoot 'profile-a-stderr.log')
  Start-Sleep -Seconds 3
  & (Join-Path $extensionRoot 'install-native-host.ps1') -Browser $Browser -ExtensionId $ExtensionId -HostExecutable $hostBinary | Write-Host
  $browserB = Start-SmokeBrowser $profileB 'B' (Join-Path $profileRoot 'profile-b-stderr.log')

  $heldConnections = Get-InstanceHandshakes 2
  Assert-ProfileHost $browserA
  Assert-ProfileHost $browserB
  $identities = @($heldConnections | ForEach-Object { $_.Hello })
  $connectionA = $null
  $connectionB = $null
  foreach ($connection in $heldConnections) {
    $tabs = Get-Tabs $connection
    if (@($tabs | Where-Object { $_.url -match 'profile=A(?:$|&)' }).Count -gt 0) { $connectionA = $connection }
    if (@($tabs | Where-Object { $_.url -match 'profile=B(?:$|&)' }).Count -gt 0) { $connectionB = $connection }
  }
  if (-not $connectionA -or -not $connectionB) { throw 'could not map each Native Messaging identity to its profile-specific page' }
  Close-Handshakes $heldConnections
  $heldConnections = @()
  if (@($identities.installation_id | Sort-Object -Unique).Count -ne 2 -or @($identities.profile_key | Sort-Object -Unique).Count -ne 2 -or @($identities.boot_id | Sort-Object -Unique).Count -ne 2) {
    throw 'two browser Profiles did not receive independent installation/profile/boot identities'
  }
  $helloA = $connectionA.Hello
  $helloB = $connectionB.Hello
  Invoke-ProfileSmoke $helloA 'A' $browserA.Url
  Invoke-ProfileSmoke $helloB 'B' $browserB.Url

  if (-not (Stop-SmokeBrowser $browserA)) { throw 'Profile A Edge/Chrome process did not exit before restart' }
  $browserA = $null
  Invoke-ProfileSmoke $helloB 'B' $browserB.Url

  $browserA = Start-SmokeBrowser $profileA 'A' (Join-Path $profileRoot 'profile-a-restarted-stderr.log')
  $heldConnections = Get-InstanceHandshakes 2
  Assert-ProfileHost $browserA
  Assert-ProfileHost $browserB
  $restartedIdentities = @($heldConnections | ForEach-Object { $_.Hello })
  $restartedAConnection = $null
  $currentBConnection = $null
  foreach ($connection in $heldConnections) {
    $tabs = Get-Tabs $connection
    if (@($tabs | Where-Object { $_.url -match 'profile=A(?:$|&)' }).Count -gt 0) { $restartedAConnection = $connection }
    if (@($tabs | Where-Object { $_.url -match 'profile=B(?:$|&)' }).Count -gt 0) { $currentBConnection = $connection }
  }
  if (-not $restartedAConnection -or -not $currentBConnection) { throw 'could not remap restarted Native Messaging identities to the original pages' }
  Close-Handshakes $heldConnections
  $heldConnections = @()
  $newHelloA = $restartedAConnection.Hello
  $currentHelloB = $currentBConnection.Hello
  if (-not $newHelloA -or $newHelloA.profile_key -ne $helloA.profile_key -or $newHelloA.boot_id -eq $helloA.boot_id) {
    throw 'Profile A restart did not preserve installation/profile identity and issue a fresh boot_id'
  }
  if (-not $currentHelloB -or $currentHelloB.boot_id -ne $helloB.boot_id) { throw 'Profile A restart changed Profile B identity' }

  $staleArgs = @(
    'browser-extension-smoke', '--family', $Browser, '--pipe', $pipePath,
    '--url', $browserA.Url,
    '--extension-id', $helloA.extension_id,
    '--installation-id', $helloA.installation_id,
    '--profile-key', $helloA.profile_key,
    '--boot-id', $helloA.boot_id
  )
  $savedErrorAction = $ErrorActionPreference
  try {
    $ErrorActionPreference = 'Continue'
    $staleOutput = & $rivo @staleArgs 2>&1
    $staleCode = $LASTEXITCODE
  } finally {
    $ErrorActionPreference = $savedErrorAction
  }
  if ($staleCode -eq 0 -or ($staleOutput -join "`n") -notmatch '身份不匹配') {
    throw 'old Profile A boot_id was not rejected after restart'
  }
  Write-Output 'browser extension stale boot_id rejection: OK'
  Invoke-ProfileSmoke $newHelloA 'A' $browserA.Url
  Invoke-ProfileSmoke $currentHelloB 'B' $browserB.Url
  Write-Output 'browser extension real two-profile isolation, host-registration recovery, and restart smoke: OK'
  Write-Output "profile-A installation=$($newHelloA.installation_id) profile=$($newHelloA.profile_key) old_boot=$($helloA.boot_id) new_boot=$($newHelloA.boot_id)"
  Write-Output "profile-B installation=$($currentHelloB.installation_id) profile=$($currentHelloB.profile_key) boot=$($currentHelloB.boot_id)"
}
finally {
  Close-Handshakes $heldConnections
  Stop-SmokeBrowser $browserA
  Stop-SmokeBrowser $browserB
  if ($server -and -not $server.HasExited) { Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue }
  if ($hadRegistry) {
    New-Item -Force -Path $registryPath | Out-Null
    Set-ItemProperty -LiteralPath $registryPath -Name '(Default)' -Value $oldRegistryManifest
  } elseif (Test-Path -LiteralPath $registryPath) {
    Remove-Item -LiteralPath $registryPath -Force
  }
  foreach ($manifestPath in $manifestBackups.Keys) { [IO.File]::WriteAllBytes($manifestPath, $manifestBackups[$manifestPath]) }
  foreach ($fallbackPath in $fallbackRegistryBackups.Keys) {
    New-Item -Force -Path $fallbackPath | Out-Null
    Set-ItemProperty -LiteralPath $fallbackPath -Name '(Default)' -Value $fallbackRegistryBackups[$fallbackPath]
  }
  if (-not $manifestBackups.ContainsKey($managedManifest) -and (Test-Path -LiteralPath $managedManifest)) {
    Remove-Item -LiteralPath $managedManifest -Force
  }
  if ($null -eq $oldPipe) { Remove-Item Env:RIVO_BROWSER_EXTENSION_PIPE -ErrorAction SilentlyContinue }
  else { $env:RIVO_BROWSER_EXTENSION_PIPE = $oldPipe }
  if ($KeepProfiles) { Write-Output "kept browser profile test data: $profileRoot" }
  elseif (Test-Path -LiteralPath $profileRoot) { Remove-Item -LiteralPath $profileRoot -Recurse -Force -ErrorAction SilentlyContinue }
}

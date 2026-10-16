param(
  [string]$HostExecutable = "$(Join-Path $PSScriptRoot '..\runtime\target\debug\rivo-browser-extension-host.exe')"
)

$ErrorActionPreference = 'Stop'
$resolvedHost = (Resolve-Path -LiteralPath $HostExecutable).Path
$pipeName = "rivo-browser-extension-multi-$([guid]::NewGuid().ToString('N'))"
$pipePath = "\\.\pipe\$pipeName"

function Encode-Frame($value) {
  $json = $value | ConvertTo-Json -Compress -Depth 10
  $body = [Text.Encoding]::UTF8.GetBytes($json)
  $frame = New-Object byte[] (4 + $body.Length)
  [BitConverter]::GetBytes([uint32]$body.Length).CopyTo($frame, 0)
  [Array]::Copy($body, 0, $frame, 4, $body.Length)
  $frame
}

function Read-Frame($stream) {
  $header = New-Object byte[] 4
  if ($stream.Read($header, 0, 4) -ne 4) { throw 'short Native Messaging header' }
  $length = [BitConverter]::ToUInt32($header, 0)
  if ($length -gt 1048576) { throw 'Native Messaging frame exceeds 1 MiB' }
  $body = New-Object byte[] $length
  $offset = 0
  while ($offset -lt $length) {
    $read = $stream.Read($body, $offset, $length - $offset)
    if ($read -le 0) { throw 'short Native Messaging body' }
    $offset += $read
  }
  [Text.Encoding]::UTF8.GetString($body) | ConvertFrom-Json
}

function Start-Host($hello) {
  $psi = [Diagnostics.ProcessStartInfo]::new()
  $psi.FileName = $resolvedHost
  $psi.UseShellExecute = $false
  $psi.RedirectStandardInput = $true
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  $psi.CreateNoWindow = $true
  $psi.Environment['RIVO_BROWSER_EXTENSION_PIPE'] = $pipePath
  $process = [Diagnostics.Process]::new()
  $process.StartInfo = $psi
  if (-not $process.Start()) { throw 'failed to start Native Messaging host' }
  $frame = Encode-Frame $hello
  $process.StandardInput.BaseStream.Write($frame, 0, $frame.Length)
  $process.StandardInput.BaseStream.Flush()
  [pscustomobject]@{ Process = $process; Hello = $hello }
}

$instances = @(
  [ordered]@{ browser_family = 'chrome'; extension_id = ('a' * 32); installation_id = 'multi-install-1'; profile_key = 'profile-1'; boot_id = 'multi-boot-1' },
  [ordered]@{ browser_family = 'chrome'; extension_id = ('b' * 32); installation_id = 'multi-install-2'; profile_key = 'profile-2'; boot_id = 'multi-boot-2' }
)
$processes = @()
$pipes = @()
try {
  foreach ($instance in $instances) {
    $hello = [ordered]@{ protocol = 'rivo-browser-extension-v1'; browser_family = $instance.browser_family; extension_id = $instance.extension_id; installation_id = $instance.installation_id; profile_key = $instance.profile_key; boot_id = $instance.boot_id; capabilities = @('browser_list_tabs') }
    $processes += Start-Host $hello
  }
  Start-Sleep -Milliseconds 150
  foreach ($expected in $instances) {
    $pipe = [IO.Pipes.NamedPipeClientStream]::new('.', $pipeName, [IO.Pipes.PipeDirection]::InOut, [IO.Pipes.PipeOptions]::Asynchronous)
    $pipe.Connect(5000)
    $hello = Read-Frame $pipe
    if ($hello.boot_id -notin @('multi-boot-1', 'multi-boot-2')) { throw 'unexpected multi-instance handshake' }
    $pipes += [pscustomobject]@{ Stream = $pipe; Hello = $hello }
  }
  if (($pipes | ForEach-Object { $_.Hello.boot_id } | Sort-Object -Unique).Count -ne 2) { throw 'named pipe instances were not isolated' }
  foreach ($connection in $pipes) {
    $request = [ordered]@{ protocol = 'rivo-browser-extension-v1'; id = "request-$($connection.Hello.boot_id)"; method = 'browser_list_tabs'; browser_instance = $connection.Hello | Select-Object browser_family,extension_id,installation_id,profile_key,boot_id; params = @{}; deadline_ms = 20000 }
    $frame = Encode-Frame $request
    $connection.Stream.Write($frame, 0, $frame.Length)
    $connection.Stream.Flush()
    $forwarded = $null
    foreach ($process in $processes) {
      if ($process.Process.HasExited) { continue }
      if ($process.Hello.boot_id -eq $connection.Hello.boot_id) {
        $owner = $process.Process
        $forwarded = Read-Frame $process.Process.StandardOutput.BaseStream
        break
      }
    }
    if (-not $forwarded) { throw 'multi-instance request was not forwarded' }
    $response = [ordered]@{ protocol = 'rivo-browser-extension-v1'; id = $request.id; ok = $true; result = @(@{id='tab-1'; title=$connection.Hello.profile_key; url='https://example.test/'}); browser_instance = $request.browser_instance }
    $responseFrame = Encode-Frame $response
    $owner.StandardInput.BaseStream.Write($responseFrame, 0, $responseFrame.Length)
    $owner.StandardInput.BaseStream.Flush()
    $result = Read-Frame $connection.Stream
    if (-not $result.ok -or $result.browser_instance.boot_id -ne $connection.Hello.boot_id) { throw 'multi-instance response identity failed' }
  }
  Write-Output 'browser extension multi-instance host smoke: OK'
  # Browser EOF must remove an idle host, with or without a connected Rivo
  # client. Closing one Profile must leave the other Profile operational.
  $firstHost = $processes[0].Process
  $firstHost.StandardInput.Close()
  if (-not $firstHost.WaitForExit(5000)) { throw 'closed browser left a connected idle Native Host alive' }
  $survivor = $pipes | Where-Object { $_.Hello.boot_id -eq $processes[1].Hello.boot_id } | Select-Object -First 1
  $survivorRequest = [ordered]@{ protocol = 'rivo-browser-extension-v1'; id = 'surviving-profile'; method = 'browser_list_tabs'; browser_instance = $survivor.Hello | Select-Object browser_family,extension_id,installation_id,profile_key,boot_id; params = @{}; deadline_ms = 20000 }
  $frame = Encode-Frame $survivorRequest
  $survivor.Stream.Write($frame, 0, $frame.Length)
  $survivor.Stream.Flush()
  $forwarded = Read-Frame $processes[1].Process.StandardOutput.BaseStream
  if ($forwarded.id -ne $survivorRequest.id) { throw 'closing one Profile broke the surviving Profile' }
  $response = [ordered]@{ protocol = 'rivo-browser-extension-v1'; id = $survivorRequest.id; ok = $true; result = @(); browser_instance = $survivorRequest.browser_instance }
  $frame = Encode-Frame $response
  $processes[1].Process.StandardInput.BaseStream.Write($frame, 0, $frame.Length)
  $processes[1].Process.StandardInput.BaseStream.Flush()
  $result = Read-Frame $survivor.Stream
  if (-not $result.ok -or $result.id -ne $survivorRequest.id) { throw 'surviving Profile response failed' }
  $survivor.Stream.Dispose()
  Start-Sleep -Milliseconds 150
  $processes[1].Process.StandardInput.Close()
  if (-not $processes[1].Process.WaitForExit(5000)) { throw 'closed browser left a listening idle Native Host alive' }
  Write-Output 'browser extension idle EOF, listening EOF, and surviving Profile smoke: OK'
}
finally {
  foreach ($pipe in $pipes) { $pipe.Stream.Dispose() }
  foreach ($process in $processes) { if (-not $process.Process.HasExited) { $process.Process.Kill() }; $process.Process.Dispose() }
}

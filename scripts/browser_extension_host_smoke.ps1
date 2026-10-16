param(
  [string]$HostExecutable = "$(Join-Path $PSScriptRoot '..\runtime\target\debug\rivo-browser-extension-host.exe')"
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $HostExecutable)) {
  throw "Native Messaging host executable not found: $HostExecutable"
}

$pipeName = "rivo-browser-extension-smoke-$([guid]::NewGuid().ToString('N'))"
$pipePath = "\\.\pipe\$pipeName"
$instance = [ordered]@{
  browser_family = 'chrome'
  extension_id = ('a' * 32)
  installation_id = 'smoke-installation'
  profile_key = 'smoke-profile'
  boot_id = 'smoke-boot'
}

function Encode-Frame($value) {
  $json = $value | ConvertTo-Json -Compress -Depth 10
  $body = [Text.Encoding]::UTF8.GetBytes($json)
  $frame = New-Object byte[] (4 + $body.Length)
  [BitConverter]::GetBytes([uint32]$body.Length).CopyTo($frame, 0)
  [Array]::Copy($body, 0, $frame, 4, $body.Length)
  return $frame
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
  return [Text.Encoding]::UTF8.GetString($body) | ConvertFrom-Json
}

$hello = [ordered]@{
  protocol = 'rivo-browser-extension-v1'
  browser_family = 'chrome'
  extension_id = $instance.extension_id
  installation_id = $instance.installation_id
  profile_key = $instance.profile_key
  boot_id = $instance.boot_id
  capabilities = @('browser_list_tabs', 'browser_navigate')
}
$request = [ordered]@{
  protocol = 'rivo-browser-extension-v1'
  id = 'smoke-request-1'
  method = 'browser_list_tabs'
  browser_instance = $instance
  params = @{}
  deadline_ms = 20000
}
$response = [ordered]@{
  protocol = 'rivo-browser-extension-v1'
  id = 'smoke-request-1'
  ok = $true
  result = @(@{ id = 'tab-1'; title = 'Smoke'; url = 'https://example.test/' })
  browser_instance = $instance
}

$psi = [Diagnostics.ProcessStartInfo]::new()
$psi.FileName = (Resolve-Path -LiteralPath $HostExecutable).Path
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.CreateNoWindow = $true
$psi.Environment['RIVO_BROWSER_EXTENSION_PIPE'] = $pipePath
$process = [Diagnostics.Process]::new()
$process.StartInfo = $psi
if (-not $process.Start()) { throw 'failed to start Native Messaging host' }

try {
  $process.StandardInput.BaseStream.Write((Encode-Frame $hello), 0, (Encode-Frame $hello).Length)
  $process.StandardInput.BaseStream.Flush()
  $pipe = [IO.Pipes.NamedPipeClientStream]::new('.', $pipeName, [IO.Pipes.PipeDirection]::InOut, [IO.Pipes.PipeOptions]::Asynchronous)
  $pipe.Connect(5000)
  $pipeHello = Read-Frame $pipe
  if ($pipeHello.protocol -ne 'rivo-browser-extension-v1' -or $pipeHello.boot_id -ne 'smoke-boot') { throw 'host handshake identity mismatch' }
  $requestFrame = Encode-Frame $request
  $pipe.Write($requestFrame, 0, $requestFrame.Length)
  $pipe.Flush()
  $forwarded = Read-Frame $process.StandardOutput.BaseStream
  if ($forwarded.method -ne 'browser_list_tabs' -or $forwarded.id -ne 'smoke-request-1') { throw 'host did not forward request' }
  $responseFrame = Encode-Frame $response
  $process.StandardInput.BaseStream.Write($responseFrame, 0, $responseFrame.Length)
  $process.StandardInput.BaseStream.Flush()
  $result = Read-Frame $pipe
  if (-not $result.ok -or $result.result[0].url -ne 'https://example.test/') { throw 'host response round trip failed' }

  $timeoutRequest = [ordered]@{
    protocol = 'rivo-browser-extension-v1'
    id = 'smoke-request-timeout'
    method = 'browser_snapshot'
    browser_instance = $instance
    params = @{}
    deadline_ms = 250
  }
  $timeoutFrame = Encode-Frame $timeoutRequest
  $clock = [Diagnostics.Stopwatch]::StartNew()
  $pipe.Write($timeoutFrame, 0, $timeoutFrame.Length)
  $pipe.Flush()
  $forwardedTimeout = Read-Frame $process.StandardOutput.BaseStream
  if ($forwardedTimeout.id -ne $timeoutRequest.id) { throw 'host timeout request identity mismatch' }
  $timeoutResult = Read-Frame $pipe
  $clock.Stop()
  if ($timeoutResult.ok -or $timeoutResult.id -ne $timeoutRequest.id) { throw 'host did not return a structured timeout response' }
  if ($timeoutResult.browser_instance.boot_id -ne $instance.boot_id) { throw 'host timeout response instance mismatch' }
  if ($timeoutResult.error -notmatch 'deadline') { throw "unexpected host timeout error: $($timeoutResult.error)" }
  if ($clock.ElapsedMilliseconds -gt 5000) { throw "host timeout exceeded 5 seconds: $($clock.ElapsedMilliseconds)ms" }
  if (-not $process.WaitForExit(5000)) { throw 'host did not close the timed-out native session' }

  Write-Output 'browser extension native host smoke: OK'
  Write-Output "browser extension native host deadline smoke: OK ($($clock.ElapsedMilliseconds)ms)"
}
finally {
  if ($pipe) { $pipe.Dispose() }
  if (-not $process.HasExited) { $process.Kill() }
  $process.Dispose()
}

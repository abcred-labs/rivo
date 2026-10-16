param(
  [Parameter(Mandatory = $true)][ValidateSet('chrome','edge')][string]$Browser,
  [ValidatePattern('^[a-p]{32}$')][string]$ExtensionId = 'adnanfbbgelpelombhdjflmcggepbcee',
  [string]$HostExecutable = "$env:ProgramFiles\Rivo\rivo-browser-extension-host.exe",
  [string]$PipeName = ""
)

$ErrorActionPreference = 'Stop'
$hostName = 'com.rivo.browser'
$manifestRoot = Join-Path $env:LOCALAPPDATA 'Rivo\NativeMessagingHosts'
New-Item -ItemType Directory -Force -Path $manifestRoot | Out-Null
$manifestPath = Join-Path $manifestRoot "$hostName.$Browser.json"
$manifest = [ordered]@{
  name = $hostName
  description = 'Rivo browser extension Native Messaging host'
  path = (Resolve-Path $HostExecutable).Path
  type = 'stdio'
  allowed_origins = @("chrome-extension://$ExtensionId/")
}
$manifestJson = $manifest | ConvertTo-Json -Depth 4
[System.IO.File]::WriteAllText($manifestPath, $manifestJson, [System.Text.UTF8Encoding]::new($false))
$registryPath = if ($Browser -eq 'chrome') { 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\com.rivo.browser' } else { 'HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\com.rivo.browser' }
New-Item -Force -Path $registryPath | Out-Null
Set-ItemProperty -Path $registryPath -Name '(Default)' -Value $manifestPath
Write-Output "Registered $Browser Native Messaging host: $manifestPath"

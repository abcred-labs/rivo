<#
.SYNOPSIS
  Run the deterministic portion of the FEAT-030 Windows Terminal acceptance check.

.DESCRIPTION
  The script uses an isolated RIVO_CONFIG/RIVO_DATA_DIR, exercises text/table/json
  query output, default-format precedence, empty results, and invalid argument
  handling. It writes a redacted transcript and a JSON/Markdown summary.

  It deliberately does not claim visual acceptance: the operator must inspect the
  printed table output in Windows Terminal at normal and narrow window widths and
  attach screenshots to the evidence directory.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File .\scripts\feat030_windows_terminal_check.ps1 -Build
#>
[CmdletBinding()]
param(
    [string]$Rivo,
    [string]$OutputDirectory,
    [string]$CandidateCommit = '7ca73ac65ed8ad25c1c6755ca3ff572c6b7b83cf',
    [switch]$Build
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Windows PowerShell 5.1 decodes native stdout with Console.OutputEncoding. Keep
# the script's native Rivo pipeline UTF-8, then restore the caller's encodings.
$previousOutputEncoding = $OutputEncoding
$previousConsoleOutputEncoding = [Console]::OutputEncoding
$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$OutputEncoding = $utf8NoBom
[Console]::OutputEncoding = $utf8NoBom
$zhipuDisplayName = ([char]0x667A, [char]0x8C31) -join ''

function Fail([string]$Message) {
    throw "FEAT-030 check: $Message"
}

function Redact([string]$Text) {
    $result = $Text
    $result = [regex]::Replace($result, '(?i)(authorization\s*[:=]\s*(?:bearer\s+)?)\S+', '$1<REDACTED>')
    $result = [regex]::Replace($result, '(?i)(api[_-]?key\s*[:=]\s*)\S+', '$1<REDACTED>')
    $result = [regex]::Replace($result, '(?i)(credential\s*[:=]\s*)\S+', '$1<REDACTED>')
    return $result
}

$repoRoot = Split-Path -Parent $PSScriptRoot
if ([string]::IsNullOrWhiteSpace($Rivo)) {
    $Rivo = Join-Path $repoRoot 'runtime\target\debug\rivo.exe'
}

if ($Build) {
    Push-Location (Join-Path $repoRoot 'runtime')
    try {
        & cargo build -p rivo --locked
        if ($LASTEXITCODE -ne 0) { Fail "cargo build failed with exit code $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
}

if (-not (Test-Path -LiteralPath $Rivo -PathType Leaf)) {
    Fail "rivo executable was not found: $Rivo (pass -Rivo or use -Build)"
}
$Rivo = (Resolve-Path -LiteralPath $Rivo).Path

if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $OutputDirectory = Join-Path $repoRoot "artifacts\feat030-windows-terminal-$stamp"
}
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path

$sandboxRoot = Join-Path $OutputDirectory 'isolated-state'
New-Item -ItemType Directory -Force -Path (Join-Path $sandboxRoot 'data') | Out-Null
$configPath = Join-Path $sandboxRoot 'config.toml'
$dataPath = Join-Path $sandboxRoot 'data'

$previousConfig = $env:RIVO_CONFIG
$previousData = $env:RIVO_DATA_DIR
$env:RIVO_CONFIG = $configPath
$env:RIVO_DATA_DIR = $dataPath

$results = [System.Collections.Generic.List[object]]::new()
$transcript = [System.Collections.Generic.List[string]]::new()

function Invoke-Case {
    param(
        [string]$Name,
        [string[]]$Arguments,
        [int]$ExpectedExit = 0
    )

    $display = '& rivo ' + ($Arguments -join ' ')
    Write-Verbose $display
    $savedErrorAction = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = (& $Rivo @Arguments 2>&1 | Out-String).TrimEnd()
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $savedErrorAction
    }
    $redacted = Redact $output
    $transcript.Add("`n## $Name`n`n~~~text`n$display`nexit_code=$exitCode`n$redacted`n~~~")
    $passed = $exitCode -eq $ExpectedExit
    $results.Add([pscustomobject]@{
            name = $Name
            command = $display
            exit_code = $exitCode
            expected_exit = $ExpectedExit
            passed = $passed
        })
    if (-not $passed) {
        Fail "$Name expected exit $ExpectedExit but got $exitCode"
    }
    return $output
}

function Assert-Check {
    param([bool]$Condition, [string]$Name, [string]$Message)
    $results.Add([pscustomobject]@{
            name = $Name
            command = 'assertion'
            exit_code = 0
            expected_exit = 0
            passed = $Condition
        })
    if (-not $Condition) { Fail "$Name failed: $Message" }
}

try {
    $version = Invoke-Case 'version' @('--version')
    $text = Invoke-Case 'vendor-list-default-text' @('config', 'vendor', 'list')
    Assert-Check ($text -match "`t") 'default-text-tabs' 'default query output must use Tab-separated text'
    Assert-Check (-not $text.TrimStart().StartsWith('+')) 'default-text-not-table' 'default query output must not be an ASCII table'
    Assert-Check (-not $text.TrimStart().StartsWith('[')) 'default-text-not-json' 'default query output must not be JSON'
    $expectedTextLine = "zhipu`t$zhipuDisplayName`tsource=Builtin"
    Assert-Check (@($text -split "`r?`n" | Where-Object { $_ -ceq $expectedTextLine }).Count -eq 1) 'default-text-cjk-utf8' 'zhipu display name must remain the expected CJK value in text output'

    $table = Invoke-Case 'vendor-list-explicit-table' @('config', 'vendor', 'list', '--format', 'table')
    Assert-Check ($table.TrimStart().StartsWith('+')) 'table-border' 'table must start with an ASCII border'
    Assert-Check ($table -match '(?m)^\| ID\s*\|') 'table-header' 'table must include an ID header'
    Assert-Check ($table -match '(?m)^\| [^|]+\|') 'table-rows' 'table must include bordered rows'
    $expectedTablePattern = '^\| zhipu +\| ' + [regex]::Escape($zhipuDisplayName) + ' +\| Builtin \|$'
    Assert-Check (@($table -split "`r?`n" | Where-Object { $_ -cmatch $expectedTablePattern }).Count -eq 1) 'table-cjk-utf8' 'zhipu display name must remain the expected CJK value in table output'

    $jsonText = Invoke-Case 'vendor-list-explicit-json' @('config', 'vendor', 'list', '--format', 'json')
    $jsonValue = $jsonText | ConvertFrom-Json
    Assert-Check ($jsonValue -is [System.Array]) 'vendor-list-json-array' 'list output must be a JSON array'
    $zhipuJson = @($jsonValue | Where-Object { $_.id -eq 'zhipu' })
    Assert-Check ($zhipuJson.Count -eq 1 -and $zhipuJson[0].display_name -ceq $zhipuDisplayName) 'json-cjk-utf8' 'zhipu display name must remain the expected CJK value in JSON output'

    $modelList = Invoke-Case 'model-list-json' @('config', 'model', 'list', '--format', 'json')
    $decodedModels = $modelList | ConvertFrom-Json
    $firstModel = if ($decodedModels -is [System.Array]) { $decodedModels[0] } else { $decodedModels }
    Assert-Check ($null -ne $firstModel) 'model-list-nonempty' 'built-in model catalog must not be empty'
    $modelId = [string]($firstModel.id)
    Assert-Check (-not [string]::IsNullOrWhiteSpace($modelId)) 'model-list-first-id' 'first model must expose a non-empty ID'
    $modelDetail = Invoke-Case 'model-show-json' @('config', 'model', 'show', $modelId, '--format', 'json')
    $modelObject = $modelDetail | ConvertFrom-Json
    Assert-Check ($modelObject -isnot [System.Array]) 'model-show-json-object' 'detail output must be a JSON object'
    Assert-Check ([string]$modelObject.id -eq $modelId) 'model-show-json-id' 'detail object must retain its ID'

    Invoke-Case 'set-default-table' @('config', 'set', 'query_format', 'table') | Out-Null
    $defaultTable = Invoke-Case 'vendor-list-default-table' @('config', 'vendor', 'list')
    Assert-Check ($defaultTable.TrimStart().StartsWith('+')) 'configured-default-table' 'query_format=table must affect queries'
    $explicitText = Invoke-Case 'vendor-list-explicit-text-overrides-table' @('config', 'vendor', 'list', '--format', 'text')
    Assert-Check ($explicitText -match "`t") 'explicit-text-overrides-table' 'explicit text must override configured table'
    $explicitJson = Invoke-Case 'vendor-list-explicit-json-overrides-table' @('config', 'vendor', 'list', '--format', 'json')
    Assert-Check (($explicitJson | ConvertFrom-Json) -is [System.Array]) 'explicit-json-overrides-table' 'explicit json must override configured table'
    Invoke-Case 'unset-default-format' @('config', 'unset', 'query_format') | Out-Null
    $resetText = Invoke-Case 'vendor-list-default-restored-text' @('config', 'vendor', 'list')
    Assert-Check ($resetText -match "`t") 'unset-restores-text' 'unsetting query_format must restore text'

    $emptySessionsTable = Invoke-Case 'empty-sessions-table' @('sessions', 'list', '--format', 'table')
    Assert-Check ($emptySessionsTable.TrimStart().StartsWith('+')) 'empty-table-border' 'empty list must still render a table'
    $emptySessionsJson = Invoke-Case 'empty-sessions-json' @('sessions', 'list', '--format', 'json')
    Assert-Check (($emptySessionsJson | ConvertFrom-Json) -is [System.Array]) 'empty-list-json-array' 'empty list must still be a JSON array'
    $mcpJson = Invoke-Case 'mcp-list-json' @('mcp', 'list', '--format', 'json')
    Assert-Check (($mcpJson | ConvertFrom-Json) -is [System.Array]) 'mcp-list-json-array' 'MCP list must be a JSON array'

    $configBeforeInvalid = if (Test-Path -LiteralPath $configPath) { Get-Content -Raw -LiteralPath $configPath } else { '' }
    Invoke-Case 'invalid-format-rejected' @('config', 'vendor', 'list', '--format', 'yaml') 2 | Out-Null
    Invoke-Case 'duplicate-format-rejected' @('config', 'vendor', 'list', '--format', 'json', '--format', 'table') 2 | Out-Null
    $configAfterInvalid = if (Test-Path -LiteralPath $configPath) { Get-Content -Raw -LiteralPath $configPath } else { '' }
    Assert-Check ($configBeforeInvalid -ceq $configAfterInvalid) 'invalid-format-zero-write' 'invalid query arguments must not modify config'
    Invoke-Case 'run-table-rejected' @('run', '--format', 'table', '--prompt', 'Reply with OK only.') 2 | Out-Null

    $forbidden = '(?i)(authorization\s*[:=]\s*(?!<REDACTED>)\S+|api[_-]?key\s*[:=]\s*(?!<REDACTED>)\S+|credential\s*[:=]\s*(?!<REDACTED>)\S+)'
    Assert-Check (-not (($transcript -join "`n") -match $forbidden)) 'transcript-redacted' 'transcript must not contain obvious secret labels with values'
} finally {
    if ($null -eq $previousConfig) { Remove-Item Env:RIVO_CONFIG -ErrorAction SilentlyContinue } else { $env:RIVO_CONFIG = $previousConfig }
    if ($null -eq $previousData) { Remove-Item Env:RIVO_DATA_DIR -ErrorAction SilentlyContinue } else { $env:RIVO_DATA_DIR = $previousData }
    $OutputEncoding = $previousOutputEncoding
    [Console]::OutputEncoding = $previousConsoleOutputEncoding
}

$passed = @($results | Where-Object { -not $_.passed }).Count -eq 0
$summary = [ordered]@{
    feature = 'FEAT-030'
    candidate_commit = $CandidateCommit
    checked_at = (Get-Date).ToString('o')
    operating_system = [System.Environment]::OSVersion.VersionString
    windows_terminal = $env:WT_SESSION -ne $null
    rivo = $Rivo
    rivo_version = $version
    automated_passed = $passed
    manual_visual_checks = @(
        'Inspect explicit table output in Windows Terminal at normal width: borders, headers, columns and CJK text must be readable.',
        'Resize Windows Terminal to approximately 80 columns and repeat the table command: output must remain readable without broken alignment.',
        'Capture screenshots of normal-width and narrow-width table output and place them in this evidence directory.'
    )
    cases = $results
}

$transcriptPath = Join-Path $OutputDirectory 'feat030-transcript.md'
$summaryPath = Join-Path $OutputDirectory 'feat030-summary.json'
$manualPath = Join-Path $OutputDirectory 'MANUAL-VISUAL-CHECKS.md'
Set-Content -LiteralPath $transcriptPath -Value ("# FEAT-030 automated transcript`n`nCandidate: $CandidateCommit`n" + ($transcript -join "`n")) -Encoding utf8
$summary | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $summaryPath -Encoding utf8
Set-Content -LiteralPath $manualPath -Value @"
# FEAT-030 Windows Terminal visual checks

Candidate: $CandidateCommit

Automated checks passed. Before accepting FEAT-030, run this command in Windows Terminal at a normal width and again at approximately 80 columns:

~~~powershell
& '$Rivo' config vendor list --format table
~~~

Confirm that ASCII borders, the `ID` header, columns and CJK content are readable and aligned. Save one screenshot for each width beside this file, then record the reviewer, date and pass/fail result below.

- Reviewer:
- Date:
- Normal width: pass / fail
- Narrow width: pass / fail
- Notes:
"@ -Encoding utf8

Write-Host "FEAT-030 automated checks passed."
Write-Host "Evidence directory: $OutputDirectory"
Write-Host "Manual visual checklist: $manualPath"
Write-Host "Review normal-width and approximately 80-column table output before closing FEAT-030."

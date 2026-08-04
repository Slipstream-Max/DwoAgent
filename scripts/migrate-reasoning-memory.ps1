<#
.SYNOPSIS
Adds per-model reasoning memory to persisted dwo sessions.

.DESCRIPTION
Stop the dwo daemon before running this script. Existing migrated sessions are
left unchanged, so the script can be run more than once.

.EXAMPLE
pwsh -NoProfile -File .\scripts\migrate-reasoning-memory.ps1 -WhatIf

.EXAMPLE
pwsh -NoProfile -File .\scripts\migrate-reasoning-memory.ps1
#>

[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string] $ProfileRoot = (Join-Path ([Environment]::GetFolderPath('UserProfile')) '.dwoagent')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$sessionRoot = Join-Path $ProfileRoot 'runtime\sessions'
if (-not (Test-Path -LiteralPath $sessionRoot -PathType Container)) {
    throw "Session directory does not exist: $sessionRoot"
}

$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$scanned = 0
$migrated = 0
$unchanged = 0

$sessionFiles = Get-ChildItem -LiteralPath $sessionRoot -Recurse -File -Filter 'session.json'
foreach ($sessionFile in $sessionFiles) {
    $scanned++

    $source = [System.IO.File]::ReadAllText($sessionFile.FullName)
    $metadata = $source | ConvertFrom-Json -Depth 100
    if ($null -eq $metadata.llm) {
        throw "Session metadata is missing llm: $($sessionFile.FullName)"
    }

    $modelProperty = $metadata.llm.PSObject.Properties['model']
    if ($null -eq $modelProperty -or
        $null -eq $modelProperty.Value -or
        [string]::IsNullOrWhiteSpace([string] $modelProperty.Value)) {
        throw "Session metadata is missing llm.model: $($sessionFile.FullName)"
    }

    if ($null -ne $metadata.llm.PSObject.Properties['reasoning_by_model']) {
        $unchanged++
        continue
    }

    $reasoningProperty = $metadata.llm.PSObject.Properties['reasoning']
    $reasoning = if ($null -eq $reasoningProperty) {
        $null
    } else {
        $reasoningProperty.Value
    }
    $memory = [ordered]@{}
    $memory[[string] $modelProperty.Value] = $reasoning
    $metadata.llm | Add-Member -MemberType NoteProperty -Name 'reasoning_by_model' -Value ([pscustomobject] $memory)

    if (-not $PSCmdlet.ShouldProcess($sessionFile.FullName, 'Add per-model reasoning memory')) {
        continue
    }

    $json = $metadata | ConvertTo-Json -Depth 100
    $temporary = Join-Path $sessionFile.DirectoryName ('.session.json.reasoning-migration.{0}.tmp' -f [Guid]::NewGuid().ToString('N'))
    try {
        [System.IO.File]::WriteAllText($temporary, "$json`n", $utf8NoBom)
        [System.IO.File]::Move($temporary, $sessionFile.FullName, $true)
        $migrated++
    } finally {
        if (Test-Path -LiteralPath $temporary) {
            Remove-Item -LiteralPath $temporary -Force
        }
    }
}

[pscustomobject]@{
    ProfileRoot = [System.IO.Path]::GetFullPath($ProfileRoot)
    Scanned = $scanned
    Migrated = $migrated
    Unchanged = $unchanged
}

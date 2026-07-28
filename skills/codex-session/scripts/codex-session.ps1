[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet("list", "prompt", "watch", "cancel", "__worker")]
    [string]$Command,
    [string]$Message,
    [string]$Cwd,
    [string]$Model,
    [string]$Reasoning,
    [string]$To,
    [string]$SessionId,
    [ValidateRange(0, [long]::MaxValue)]
    [long]$Cursor = 0,
    [ValidateRange(1, 100)]
    [int]$Limit = 3,
    [string]$Payload
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Get-PropertyValue {
    param([AllowNull()][object]$InputObject, [Parameter(Mandatory)][string]$Name)

    if ($null -eq $InputObject) {
        return $null
    }
    if ($InputObject -is [Collections.IDictionary] -and $InputObject.Contains($Name)) {
        return $InputObject[$Name]
    }

    $property = $InputObject.PSObject.Properties[$Name]
    if ($null -ne $property) {
        return $property.Value
    }
    return $null
}

function ConvertTo-JsonLine {
    param([Parameter(Mandatory)][object]$Value)
    return ($Value | ConvertTo-Json -Depth 30 -Compress)
}

function Write-JsonResult {
    param([Parameter(Mandatory)][object]$Value)
    Write-Output (ConvertTo-JsonLine -Value $Value)
}

function Get-CodexHome {
    if (-not [string]::IsNullOrWhiteSpace($env:CODEX_HOME)) {
        return [IO.Path]::GetFullPath($env:CODEX_HOME)
    }
    $profile = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
    return (Join-Path $profile ".codex")
}

function Get-RolloutFiles {
    $root = Join-Path (Get-CodexHome) "sessions"
    if (Test-Path -LiteralPath $root -PathType Container) {
        Get-ChildItem -LiteralPath $root -Recurse -Filter "rollout-*.jsonl" -File -ErrorAction SilentlyContinue
    }
}

function Get-SessionObjectName {
    param(
        [Parameter(Mandatory)][ValidateSet("mutex", "cancel")][string]$Kind,
        [Parameter(Mandatory)][string]$Id
    )

    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $hash = $sha256.ComputeHash([Text.Encoding]::UTF8.GetBytes($Id.ToLowerInvariant()))
    }
    finally {
        $sha256.Dispose()
    }

    $hex = -join ($hash | ForEach-Object { $_.ToString("x2") })
    if ($Kind -eq "mutex") {
        return "Local\DwoCodexSession-$hex"
    }
    return "Local\DwoCodexCancel-$hex"
}

function Enter-SessionMutex {
    param([Parameter(Mandatory)][string]$Id)

    $createdNew = $false
    $mutex = [Threading.Mutex]::new(
        $false,
        (Get-SessionObjectName -Kind mutex -Id $Id),
        [ref]$createdNew
    )
    try {
        $acquired = $mutex.WaitOne(0)
    }
    catch [Threading.AbandonedMutexException] {
        $acquired = $true
    }

    if (-not $acquired) {
        $mutex.Dispose()
        return $null
    }
    return $mutex
}

function Test-SessionBusy {
    param([Parameter(Mandatory)][string]$Id)

    $mutex = $null
    $acquired = $false
    try {
        $mutex = [Threading.Mutex]::OpenExisting((Get-SessionObjectName -Kind mutex -Id $Id))
        try {
            $acquired = $mutex.WaitOne(0)
        }
        catch [Threading.AbandonedMutexException] {
            $acquired = $true
        }
        return (-not $acquired)
    }
    catch [Threading.WaitHandleCannotBeOpenedException] {
        return $false
    }
    finally {
        if ($acquired -and $null -ne $mutex) {
            $mutex.ReleaseMutex()
        }
        if ($null -ne $mutex) {
            $mutex.Dispose()
        }
    }
}

function New-CancelEvent {
    param([Parameter(Mandatory)][string]$Id)

    $createdNew = $false
    $event = [Threading.EventWaitHandle]::new(
        $false,
        [Threading.EventResetMode]::ManualReset,
        (Get-SessionObjectName -Kind cancel -Id $Id),
        [ref]$createdNew
    )
    [void]$event.Reset()
    return $event
}

function Open-RolloutReader {
    param([Parameter(Mandatory)][IO.FileInfo]$File)

    $stream = [IO.File]::Open(
        $File.FullName,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete)
    )
    return [IO.StreamReader]::new($stream, [Text.Encoding]::UTF8, $true)
}

function Find-RolloutFile {
    param([Parameter(Mandatory)][string]$Id)

    $files = @(Get-RolloutFiles)
    foreach ($file in $files) {
        if ($file.BaseName.EndsWith($Id, [StringComparison]::OrdinalIgnoreCase)) {
            return $file
        }
    }

    foreach ($file in $files) {
        $reader = $null
        try {
            $reader = Open-RolloutReader -File $file
            $line = $reader.ReadLine()
            if ([string]::IsNullOrWhiteSpace($line)) {
                continue
            }
            try {
                $record = $line | ConvertFrom-Json
            }
            catch {
                continue
            }

            $payloadObject = Get-PropertyValue -InputObject $record -Name "payload"
            $recordId = Get-PropertyValue -InputObject $payloadObject -Name "id"
            if (
                (Get-PropertyValue -InputObject $record -Name "type") -eq "session_meta" -and
                [string]::Equals([string]$recordId, $Id, [StringComparison]::OrdinalIgnoreCase)
            ) {
                return $file
            }
        }
        finally {
            if ($null -ne $reader) {
                $reader.Dispose()
            }
        }
    }
    return $null
}

function Get-RolloutSummary {
    param([Parameter(Mandatory)][IO.FileInfo]$File)

    $reader = $null
    $sessionMeta = $null
    $turnContext = $null
    $lastTurnStatus = $null
    try {
        $reader = Open-RolloutReader -File $File
        while (($line = $reader.ReadLine()) -ne $null) {
            if ([string]::IsNullOrWhiteSpace($line)) {
                continue
            }
            try {
                $record = $line | ConvertFrom-Json
            }
            catch {
                continue
            }

            $recordType = Get-PropertyValue -InputObject $record -Name "type"
            $recordPayload = Get-PropertyValue -InputObject $record -Name "payload"
            if ($recordType -eq "session_meta" -and $null -eq $sessionMeta) {
                $sessionMeta = $recordPayload
            }
            elseif ($recordType -eq "turn_context") {
                $turnContext = $recordPayload
            }
            elseif ($recordType -eq "event_msg") {
                switch (Get-PropertyValue -InputObject $recordPayload -Name "type") {
                    "task_started" { $lastTurnStatus = "running" }
                    "task_complete" { $lastTurnStatus = "completed" }
                    "turn_aborted" { $lastTurnStatus = "aborted" }
                }
            }
        }
    }
    finally {
        if ($null -ne $reader) {
            $reader.Dispose()
        }
    }

    if ($null -eq $sessionMeta) {
        return $null
    }

    $id = [string](Get-PropertyValue -InputObject $sessionMeta -Name "id")
    $turnCwd = Get-PropertyValue -InputObject $turnContext -Name "cwd"
    $metaCwd = Get-PropertyValue -InputObject $sessionMeta -Name "cwd"
    return [pscustomobject][ordered]@{
        sessionId      = $id
        createdAt      = Get-PropertyValue -InputObject $sessionMeta -Name "timestamp"
        updatedAt      = $File.LastWriteTimeUtc.ToString("o")
        cwd            = if ($null -ne $turnCwd) { $turnCwd } else { $metaCwd }
        model          = Get-PropertyValue -InputObject $turnContext -Name "model"
        reasoning      = Get-PropertyValue -InputObject $turnContext -Name "effort"
        provider       = Get-PropertyValue -InputObject $sessionMeta -Name "model_provider"
        source         = Get-PropertyValue -InputObject $sessionMeta -Name "source"
        workerStatus   = if (Test-SessionBusy -Id $id) { "running" } else { "idle" }
        lastTurnStatus = $lastTurnStatus
    }
}

function Resolve-DirectoryPath {
    param([Parameter(Mandatory)][string]$Path)

    $item = Get-Item -LiteralPath $Path
    if (-not $item.PSIsContainer) {
        throw "Working directory is not a directory: $Path"
    }
    return $item.FullName
}

function Resolve-CodexPath {
    $candidates = [Collections.Generic.List[string]]::new()
    foreach ($candidate in @($env:DWO_CODEX_PATH, $env:CODEX_CLI_PATH)) {
        if (-not [string]::IsNullOrWhiteSpace($candidate)) {
            $candidates.Add($candidate)
        }
    }

    if (-not [string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        $appBin = Join-Path $env:LOCALAPPDATA "OpenAI\Codex\bin"
        if (Test-Path -LiteralPath $appBin -PathType Container) {
            $executables = Get-ChildItem -LiteralPath $appBin -Recurse -Filter "codex.exe" -File -ErrorAction SilentlyContinue |
                Sort-Object LastWriteTimeUtc -Descending
            foreach ($executable in $executables) {
                $candidates.Add($executable.FullName)
            }
        }
    }

    foreach ($resolvedCommand in @(Get-Command codex.exe -All -ErrorAction SilentlyContinue)) {
        if (-not [string]::IsNullOrWhiteSpace($resolvedCommand.Source)) {
            $candidates.Add($resolvedCommand.Source)
        }
    }

    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($candidate in $candidates) {
        if ($seen.Add($candidate) -and (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            return (Get-Item -LiteralPath $candidate).FullName
        }
    }
    throw "Could not find a runnable Codex CLI. Set DWO_CODEX_PATH to codex.exe."
}

function Get-CurrentPowerShellPath {
    return (Get-Process -Id $PID).Path
}

function Add-ProcessArgument {
    param(
        [Parameter(Mandatory)][Diagnostics.ProcessStartInfo]$StartInfo,
        [Parameter(Mandatory)][string]$Value
    )
    [void]$StartInfo.ArgumentList.Add($Value)
}

function New-CodexArguments {
    param(
        [Parameter(Mandatory)][string]$WorkingDirectory,
        [AllowNull()][string]$RequestedModel,
        [AllowNull()][string]$RequestedReasoning,
        [AllowNull()][string]$ResumeSessionId
    )

    $arguments = [Collections.Generic.List[string]]::new()
    $arguments.Add("-C")
    $arguments.Add($WorkingDirectory)
    if (-not [string]::IsNullOrWhiteSpace($RequestedModel)) {
        $arguments.Add("-m")
        $arguments.Add($RequestedModel)
    }
    if (-not [string]::IsNullOrWhiteSpace($RequestedReasoning)) {
        $arguments.Add("-c")
        $arguments.Add("model_reasoning_effort=$(ConvertTo-Json -InputObject $RequestedReasoning -Compress)")
    }

    $arguments.Add("exec")
    if (-not [string]::IsNullOrWhiteSpace($ResumeSessionId)) {
        $arguments.Add("resume")
    }
    $arguments.Add("--json")
    $arguments.Add("--skip-git-repo-check")
    if (-not [string]::IsNullOrWhiteSpace($ResumeSessionId)) {
        $arguments.Add($ResumeSessionId)
    }
    $arguments.Add("-")
    return $arguments.ToArray()
}

function New-CodexStartInfo {
    param(
        [Parameter(Mandatory)][string]$CodexPath,
        [Parameter(Mandatory)][string[]]$Arguments,
        [Parameter(Mandatory)][string]$WorkingDirectory
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    if ([IO.Path]::GetExtension($CodexPath) -eq ".ps1") {
        $startInfo.FileName = Get-CurrentPowerShellPath
        foreach ($argument in @("-NoLogo", "-NoProfile", "-NonInteractive", "-File", $CodexPath)) {
            Add-ProcessArgument -StartInfo $startInfo -Value $argument
        }
    }
    else {
        $startInfo.FileName = $CodexPath
    }
    foreach ($argument in $Arguments) {
        Add-ProcessArgument -StartInfo $startInfo -Value $argument
    }

    $utf8 = [Text.UTF8Encoding]::new($false)
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardInputEncoding = $utf8
    $startInfo.StandardOutputEncoding = $utf8
    $startInfo.StandardErrorEncoding = $utf8
    return $startInfo
}

function Stop-ProcessTree {
    param([AllowNull()][Diagnostics.Process]$Process)

    if ($null -eq $Process -or $Process.HasExited) {
        return
    }
    try {
        $Process.Kill($true)
    }
    catch {
        if (-not $Process.HasExited) {
            $Process.Kill()
        }
    }
    [void]$Process.WaitForExit(5000)
}

function Get-ThreadIdFromCliLine {
    param([Parameter(Mandatory)][string]$Line)

    try {
        $eventObject = $Line | ConvertFrom-Json
    }
    catch {
        return $null
    }
    if ((Get-PropertyValue -InputObject $eventObject -Name "type") -notin @(
        "thread.started", "thread_started", "session.started"
    )) {
        return $null
    }
    foreach ($propertyName in @("thread_id", "session_id", "id")) {
        $value = Get-PropertyValue -InputObject $eventObject -Name $propertyName
        if (-not [string]::IsNullOrWhiteSpace([string]$value)) {
            return [string]$value
        }
    }
    return $null
}

function Send-WorkerHandshake {
    param(
        [Parameter(Mandatory)][object]$Value,
        [AllowNull()][string]$PipeName
    )

    $json = ConvertTo-JsonLine -Value $Value
    if ([string]::IsNullOrWhiteSpace($PipeName)) {
        [Console]::Out.WriteLine($json)
        [Console]::Out.Flush()
        return
    }

    $client = [IO.Pipes.NamedPipeClientStream]::new(
        ".",
        $PipeName,
        [IO.Pipes.PipeDirection]::Out,
        [IO.Pipes.PipeOptions]::Asynchronous
    )
    $writer = $null
    try {
        $client.Connect(30000)
        $writer = [IO.StreamWriter]::new($client, [Text.UTF8Encoding]::new($false), 1024, $false)
        $writer.AutoFlush = $true
        $writer.WriteLine($json)
    }
    finally {
        if ($null -ne $writer) {
            $writer.Dispose()
        }
        else {
            $client.Dispose()
        }
    }
}

function Invoke-Worker {
    param([Parameter(Mandatory)][string]$EncodedPayload)

    $handshakeSent = $false
    $sessionMutex = $null
    $cancelEvent = $null
    $codexProcess = $null
    $handshakePipe = $null
    try {
        $requestJson = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($EncodedPayload))
        $request = $requestJson | ConvertFrom-Json
        $handshakePipe = [string](Get-PropertyValue -InputObject $request -Name "handshakePipe")
        $messageText = [string](Get-PropertyValue -InputObject $request -Name "message")
        $requestedCwd = [string](Get-PropertyValue -InputObject $request -Name "cwd")
        $invocationCwd = [string](Get-PropertyValue -InputObject $request -Name "invocationCwd")
        $requestedModel = [string](Get-PropertyValue -InputObject $request -Name "model")
        $requestedReasoning = [string](Get-PropertyValue -InputObject $request -Name "reasoning")
        $resumeId = [string](Get-PropertyValue -InputObject $request -Name "to")

        $rolloutSummary = $null
        $sessionId = $null
        if (-not [string]::IsNullOrWhiteSpace($resumeId)) {
            $rolloutFile = Find-RolloutFile -Id $resumeId
            if ($null -eq $rolloutFile) {
                throw "Codex session was not found: $resumeId"
            }
            $rolloutSummary = Get-RolloutSummary -File $rolloutFile
            if ($null -eq $rolloutSummary) {
                throw "Codex session metadata could not be read: $resumeId"
            }

            $sessionId = [string]$rolloutSummary.sessionId
            $sessionMutex = Enter-SessionMutex -Id $sessionId
            if ($null -eq $sessionMutex) {
                Send-WorkerHandshake -Value ([ordered]@{ sessionId = $sessionId; status = "busy" }) -PipeName $handshakePipe
                $handshakeSent = $true
                return
            }
            $cancelEvent = New-CancelEvent -Id $sessionId
        }

        if (-not [string]::IsNullOrWhiteSpace($requestedCwd)) {
            $workingDirectory = Resolve-DirectoryPath -Path $requestedCwd
        }
        elseif ($null -ne $rolloutSummary -and -not [string]::IsNullOrWhiteSpace([string]$rolloutSummary.cwd)) {
            $workingDirectory = Resolve-DirectoryPath -Path ([string]$rolloutSummary.cwd)
        }
        else {
            $workingDirectory = Resolve-DirectoryPath -Path $invocationCwd
        }

        $codexPath = Resolve-CodexPath
        $codexArguments = @(New-CodexArguments `
            -WorkingDirectory $workingDirectory `
            -RequestedModel $requestedModel `
            -RequestedReasoning $requestedReasoning `
            -ResumeSessionId $sessionId)
        $codexStartInfo = New-CodexStartInfo `
            -CodexPath $codexPath `
            -Arguments $codexArguments `
            -WorkingDirectory $workingDirectory

        $codexProcess = [Diagnostics.Process]::new()
        $codexProcess.StartInfo = $codexStartInfo
        if (-not $codexProcess.Start()) {
            throw "Codex CLI did not start."
        }

        $stderrTask = $codexProcess.StandardError.ReadToEndAsync()
        $codexProcess.StandardInput.Write($messageText)
        $codexProcess.StandardInput.Close()
        $startupLines = [Collections.Generic.List[string]]::new()
        $lineTask = $codexProcess.StandardOutput.ReadLineAsync()
        $cancelled = $false

        while ($true) {
            if ($lineTask.IsCompleted) {
                $line = $lineTask.GetAwaiter().GetResult()
                if ($null -eq $line) {
                    break
                }
                if (-not $handshakeSent -and $startupLines.Count -lt 10) {
                    $startupLines.Add($line)
                }

                if (-not $handshakeSent) {
                    $reportedSessionId = Get-ThreadIdFromCliLine -Line $line
                    if (-not [string]::IsNullOrWhiteSpace($reportedSessionId)) {
                        if ([string]::IsNullOrWhiteSpace($sessionId)) {
                            $sessionId = $reportedSessionId
                            $sessionMutex = Enter-SessionMutex -Id $sessionId
                            if ($null -eq $sessionMutex) {
                                Send-WorkerHandshake -Value ([ordered]@{ sessionId = $sessionId; status = "busy" }) -PipeName $handshakePipe
                                $handshakeSent = $true
                                Stop-ProcessTree -Process $codexProcess
                                return
                            }
                            $cancelEvent = New-CancelEvent -Id $sessionId
                        }
                        Send-WorkerHandshake -Value ([ordered]@{ sessionId = $sessionId; status = "running" }) -PipeName $handshakePipe
                        $handshakeSent = $true
                    }
                }

                $lineTask = $codexProcess.StandardOutput.ReadLineAsync()
                continue
            }

            if ($null -ne $cancelEvent -and $cancelEvent.WaitOne(50)) {
                $cancelled = $true
                Stop-ProcessTree -Process $codexProcess
            }
            else {
                Start-Sleep -Milliseconds 50
            }
        }

        $codexProcess.WaitForExit()
        $stderr = $stderrTask.GetAwaiter().GetResult().Trim()
        if (-not $handshakeSent) {
            if ($cancelled) {
                Send-WorkerHandshake -Value ([ordered]@{ sessionId = $sessionId; status = "cancelled" }) -PipeName $handshakePipe
            }
            else {
                $details = if ($stderr) { $stderr } else { ($startupLines -join [Environment]::NewLine).Trim() }
                Send-WorkerHandshake -Value ([ordered]@{
                    sessionId = $sessionId
                    status = "error"
                    message = "Codex exited before reporting a session ID."
                    details = $details
                    exitCode = $codexProcess.ExitCode
                }) -PipeName $handshakePipe
            }
            $handshakeSent = $true
        }
    }
    catch {
        if (-not $handshakeSent) {
            Send-WorkerHandshake -Value ([ordered]@{
                sessionId = $null
                status = "error"
                message = $_.Exception.Message
            }) -PipeName $handshakePipe
            $handshakeSent = $true
        }
    }
    finally {
        if ($null -ne $codexProcess) {
            if (-not $codexProcess.HasExited) {
                Stop-ProcessTree -Process $codexProcess
            }
            $codexProcess.Dispose()
        }
        if ($null -ne $cancelEvent) {
            $cancelEvent.Dispose()
        }
        if ($null -ne $sessionMutex) {
            $sessionMutex.ReleaseMutex()
            $sessionMutex.Dispose()
        }
    }
}

function Invoke-Prompt {
    param(
        [Parameter(Mandatory)][string]$PromptMessage,
        [AllowNull()][string]$RequestedCwd,
        [AllowNull()][string]$RequestedModel,
        [AllowNull()][string]$RequestedReasoning,
        [AllowNull()][string]$TargetSessionId
    )

    $pipeName = "DwoCodexHandshake-$([Guid]::NewGuid().ToString('N'))"
    $request = [ordered]@{
        message = $PromptMessage
        cwd = $RequestedCwd
        invocationCwd = (Get-Location).Path
        model = $RequestedModel
        reasoning = $RequestedReasoning
        to = $TargetSessionId
        handshakePipe = $pipeName
    }
    $requestJson = ConvertTo-JsonLine -Value $request
    $encodedPayload = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($requestJson))

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = Get-CurrentPowerShellPath
    $startInfo.UseShellExecute = $true
    $startInfo.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    foreach ($argument in @(
        "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
        "-File", $PSCommandPath, "__worker", "-Payload", $encodedPayload
    )) {
        Add-ProcessArgument -StartInfo $startInfo -Value $argument
    }

    $pipe = [IO.Pipes.NamedPipeServerStream]::new(
        $pipeName,
        [IO.Pipes.PipeDirection]::In,
        1,
        [IO.Pipes.PipeTransmissionMode]::Byte,
        [IO.Pipes.PipeOptions]::Asynchronous
    )
    $reader = $null
    $worker = [Diagnostics.Process]::new()
    $worker.StartInfo = $startInfo
    try {
        if (-not $worker.Start()) {
            throw "Codex session worker did not start."
        }

        $connectTask = $pipe.WaitForConnectionAsync()
        if (-not $connectTask.Wait(30000)) {
            Stop-ProcessTree -Process $worker
            throw "Timed out waiting for the Codex session worker to connect."
        }

        $reader = [IO.StreamReader]::new($pipe, [Text.Encoding]::UTF8, $true, 1024, $true)
        $handshakeTask = $reader.ReadLineAsync()
        if (-not $handshakeTask.Wait(30000)) {
            Stop-ProcessTree -Process $worker
            throw "Timed out waiting for Codex to report its session ID."
        }
        $handshakeLine = $handshakeTask.GetAwaiter().GetResult()
        if ([string]::IsNullOrWhiteSpace($handshakeLine)) {
            throw "Codex session worker exited without a handshake."
        }

        try {
            return ($handshakeLine | ConvertFrom-Json)
        }
        catch {
            Stop-ProcessTree -Process $worker
            throw "Codex session worker returned an invalid handshake: $handshakeLine"
        }
    }
    finally {
        if ($null -ne $reader) {
            $reader.Dispose()
        }
        $pipe.Dispose()
        $worker.Dispose()
    }
}

function Convert-RolloutMessage {
    param(
        [Parameter(Mandatory)][object]$Record,
        [Parameter(Mandatory)][long]$LineNumber
    )

    $recordType = Get-PropertyValue -InputObject $Record -Name "type"
    $payload = Get-PropertyValue -InputObject $Record -Name "payload"
    $timestamp = Get-PropertyValue -InputObject $Record -Name "timestamp"
    $payloadType = [string](Get-PropertyValue -InputObject $payload -Name "type")

    if ($recordType -eq "event_msg") {
        switch ($payloadType) {
            "user_message" {
                return [pscustomobject][ordered]@{
                    cursor = $LineNumber
                    timestamp = $timestamp
                    kind = "user"
                    content = Get-PropertyValue -InputObject $payload -Name "message"
                }
            }
            "agent_reasoning" {
                return [pscustomobject][ordered]@{
                    cursor = $LineNumber
                    timestamp = $timestamp
                    kind = "think"
                    content = Get-PropertyValue -InputObject $payload -Name "text"
                }
            }
            "agent_message" {
                return [pscustomobject][ordered]@{
                    cursor = $LineNumber
                    timestamp = $timestamp
                    kind = "assistant"
                    phase = Get-PropertyValue -InputObject $payload -Name "phase"
                    content = Get-PropertyValue -InputObject $payload -Name "message"
                }
            }
            "task_started" {
                return [pscustomobject][ordered]@{
                    cursor = $LineNumber
                    timestamp = $timestamp
                    kind = "status"
                    content = "running"
                }
            }
            "task_complete" {
                return [pscustomobject][ordered]@{
                    cursor = $LineNumber
                    timestamp = $timestamp
                    kind = "status"
                    content = "completed"
                }
            }
            "turn_aborted" {
                return [pscustomobject][ordered]@{
                    cursor = $LineNumber
                    timestamp = $timestamp
                    kind = "status"
                    content = "aborted"
                    reason = Get-PropertyValue -InputObject $payload -Name "reason"
                }
            }
            "error" {
                return [pscustomobject][ordered]@{
                    cursor = $LineNumber
                    timestamp = $timestamp
                    kind = "error"
                    content = Get-PropertyValue -InputObject $payload -Name "message"
                }
            }
        }
        return $null
    }

    if ($recordType -ne "response_item") {
        return $null
    }

    if ($payloadType -in @("custom_tool_call_output", "function_call_output", "computer_call_output")) {
        return [pscustomobject][ordered]@{
            cursor = $LineNumber
            timestamp = $timestamp
            kind = "tool_result"
            callId = Get-PropertyValue -InputObject $payload -Name "call_id"
            content = Get-PropertyValue -InputObject $payload -Name "output"
        }
    }

    if ($payloadType -in @("custom_tool_call", "function_call", "computer_call", "local_shell_call", "web_search_call")) {
        $toolName = Get-PropertyValue -InputObject $payload -Name "name"
        if ([string]::IsNullOrWhiteSpace([string]$toolName)) {
            $toolName = $payloadType
        }

        $toolInput = Get-PropertyValue -InputObject $payload -Name "input"
        foreach ($fallbackProperty in @("arguments", "action", "command")) {
            if ($null -ne $toolInput) {
                break
            }
            $toolInput = Get-PropertyValue -InputObject $payload -Name $fallbackProperty
        }

        return [pscustomobject][ordered]@{
            cursor = $LineNumber
            timestamp = $timestamp
            kind = "tool_call"
            name = $toolName
            callId = Get-PropertyValue -InputObject $payload -Name "call_id"
            content = $toolInput
        }
    }
    return $null
}

function Invoke-ListSessions {
    param([Parameter(Mandatory)][int]$MaximumSessions)

    $files = @(Get-RolloutFiles | Sort-Object LastWriteTimeUtc -Descending)
    $sessions = [Collections.Generic.List[object]]::new()
    foreach ($file in @($files | Select-Object -First $MaximumSessions)) {
        $summary = Get-RolloutSummary -File $file
        if ($null -ne $summary) {
            $sessions.Add($summary)
        }
    }

    $orderedSessions = @($sessions | Sort-Object updatedAt -Descending)
    return [pscustomobject][ordered]@{
        count = $files.Count
        returned = $orderedSessions.Count
        hasMore = $files.Count -gt $MaximumSessions
        sessions = $orderedSessions
    }
}

function Invoke-WatchSession {
    param(
        [Parameter(Mandatory)][string]$Id,
        [Parameter(Mandatory)][bool]$CursorWasProvided,
        [long]$AfterCursor,
        [int]$MaximumEvents
    )

    $file = Find-RolloutFile -Id $Id
    if ($null -eq $file) {
        throw "Codex session was not found: $Id"
    }
    $summary = Get-RolloutSummary -File $file
    $actualSessionId = if ($null -ne $summary) { [string]$summary.sessionId } else { $Id }

    $messages = [Collections.Generic.List[object]]::new()
    $lineNumber = 0L
    $reader = $null
    try {
        $reader = Open-RolloutReader -File $file
        while (($line = $reader.ReadLine()) -ne $null) {
            $lineNumber++
            if ([string]::IsNullOrWhiteSpace($line)) {
                continue
            }
            try {
                $record = $line | ConvertFrom-Json
            }
            catch {
                continue
            }
            $messageObject = Convert-RolloutMessage -Record $record -LineNumber $lineNumber
            if ($null -ne $messageObject) {
                $messages.Add($messageObject)
            }
        }
    }
    finally {
        if ($null -ne $reader) {
            $reader.Dispose()
        }
    }

    if ($CursorWasProvided) {
        $remaining = @($messages | Where-Object cursor -GT $AfterCursor)
        $selected = @($remaining | Select-Object -First $MaximumEvents)
        $hasMore = $remaining.Count -gt $selected.Count
        $nextCursor = if ($hasMore -and $selected.Count -gt 0) {
            [long]$selected[-1].cursor
        }
        else {
            $lineNumber
        }
    }
    else {
        $selected = @($messages | Select-Object -Last $MaximumEvents)
        $hasMore = $false
        $nextCursor = $lineNumber
    }

    return [pscustomobject][ordered]@{
        sessionId = $actualSessionId
        status = if (Test-SessionBusy -Id $actualSessionId) { "running" } else { "idle" }
        cursor = $nextCursor
        hasMore = $hasMore
        messages = $selected
    }
}

function Invoke-CancelSession {
    param([Parameter(Mandatory)][string]$Id)

    $file = Find-RolloutFile -Id $Id
    $actualSessionId = $Id
    if ($null -ne $file) {
        $summary = Get-RolloutSummary -File $file
        if ($null -ne $summary) {
            $actualSessionId = [string]$summary.sessionId
        }
    }

    $eventName = Get-SessionObjectName -Kind cancel -Id $actualSessionId
    for ($attempt = 0; $attempt -lt 5; $attempt++) {
        $event = $null
        try {
            $event = [Threading.EventWaitHandle]::OpenExisting($eventName)
            [void]$event.Set()
            return [pscustomobject][ordered]@{
                sessionId = $actualSessionId
                status = "cancel-requested"
            }
        }
        catch [Threading.WaitHandleCannotBeOpenedException] {
            if (-not (Test-SessionBusy -Id $actualSessionId)) {
                break
            }
            Start-Sleep -Milliseconds 25
        }
        finally {
            if ($null -ne $event) {
                $event.Dispose()
            }
        }
    }

    return [pscustomobject][ordered]@{
        sessionId = $actualSessionId
        status = "not-running"
    }
}

if ($PSVersionTable.PSVersion.Major -lt 7) {
    Write-JsonResult -Value ([ordered]@{
        status = "error"
        message = "codex-session.ps1 requires PowerShell 7 or newer."
    })
    exit 1
}

if ($Command -eq "__worker") {
    if ([string]::IsNullOrWhiteSpace($Payload)) {
        Send-WorkerHandshake -Value ([ordered]@{
            sessionId = $null
            status = "error"
            message = "Worker payload is required."
        })
        exit 1
    }
    Invoke-Worker -EncodedPayload $Payload
    exit 0
}

try {
    switch ($Command) {
        "list" {
            $listLimit = if ($PSBoundParameters.ContainsKey("Limit")) { $Limit } else { 20 }
            Write-JsonResult -Value (Invoke-ListSessions -MaximumSessions $listLimit)
        }
        "prompt" {
            if ([string]::IsNullOrWhiteSpace($Message)) {
                throw "-Message is required for prompt."
            }
            if (-not [string]::IsNullOrWhiteSpace($Cwd)) {
                $Cwd = Resolve-DirectoryPath -Path $Cwd
            }

            $result = Invoke-Prompt `
                -PromptMessage $Message `
                -RequestedCwd $Cwd `
                -RequestedModel $Model `
                -RequestedReasoning $Reasoning `
                -TargetSessionId $To
            Write-JsonResult -Value $result
            if ((Get-PropertyValue -InputObject $result -Name "status") -eq "error") {
                exit 1
            }
        }
        "watch" {
            if ([string]::IsNullOrWhiteSpace($SessionId)) {
                throw "-SessionId is required for watch."
            }
            Write-JsonResult -Value (Invoke-WatchSession `
                -Id $SessionId `
                -CursorWasProvided $PSBoundParameters.ContainsKey("Cursor") `
                -AfterCursor $Cursor `
                -MaximumEvents $Limit)
        }
        "cancel" {
            if ([string]::IsNullOrWhiteSpace($SessionId)) {
                throw "-SessionId is required for cancel."
            }
            Write-JsonResult -Value (Invoke-CancelSession -Id $SessionId)
        }
    }
}
catch {
    Write-JsonResult -Value ([ordered]@{
        status = "error"
        message = $_.Exception.Message
    })
    exit 1
}

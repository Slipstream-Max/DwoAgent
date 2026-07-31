param(
    [string]$Repo = 'C:\Users\11307\Documents\GitHub\dwoagent',
    [string]$ProfileRoot = "$env:USERPROFILE\.dwoagent"
)

$ErrorActionPreference = 'Continue'
$bin = Join-Path $ProfileRoot 'bin\dwo.exe'
$new = Join-Path $Repo 'target\release\dwo.exe'
$log = Join-Path $ProfileRoot 'runtime\restart.log'

Function Log($msg) {
    "$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') $msg" | Out-File $log -Append
}

Log '=== restart begin ==='

# give the current turn time to finish streaming its reply
Start-Sleep -Seconds 10

# 1. graceful daemon shutdown via IPC
Log 'daemon stop'
& $bin daemon stop *>> $log
Start-Sleep -Seconds 3

# 2. wait for all dwo.exe processes to exit (releases the exe image lock)
$deadline = (Get-Date).AddSeconds(30)
while ((Get-Date) -lt $deadline) {
    if (@(Get-Process dwo -ErrorAction SilentlyContinue).Count -eq 0) { break }
    Start-Sleep -Milliseconds 500
}
$left = @(Get-Process dwo -ErrorAction SilentlyContinue)
if ($left.Count -gt 0) {
    Log "force-killing remaining dwo processes: $($left.Id -join ',')"
    $left | Stop-Process -Force
    Start-Sleep -Seconds 2
}

# 3. replace the binary with the freshly built one
Copy-Item $new $bin -Force
Log "binary replaced: $((Get-Item $bin).Length) bytes"

# 4. start the daemon with the new binary
Log 'daemon start'
& $bin daemon start *>> $log
Log '=== restart done ==='

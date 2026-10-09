param(
    [string]$Repo = 'C:\Users\11307\Documents\GitHub\dwoagent',
    [string]$ProfileRoot = "$env:USERPROFILE\.dwoagent"
)

$ErrorActionPreference = 'Continue'

# A detached 5.1 host can resolve core modules from PowerShell 7 locations
# that shadow the 5.1 copies (module autoload then fails, e.g. Get-FileHash);
# pin the module path to this host's own module directories.
$env:PSModulePath = "$PSHOME\Modules;C:\Program Files\WindowsPowerShell\Modules"

$bin = Join-Path $ProfileRoot 'bin\dwo.exe'
$new = Join-Path $Repo 'target\release\dwo.exe'
$log = Join-Path $ProfileRoot 'runtime\restart.log'

Function Log($msg) {
    "$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') $msg" | Out-File $log -Append
}

# Hash via .NET so the verified swap never depends on module autoloading.
Function Get-Sha256($path) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [IO.File]::ReadAllBytes($path)
        return ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace('-', '').ToLowerInvariant()
    } finally {
        $sha.Dispose()
    }
}

Log '=== restart begin (verified) ==='

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

# 3. replace the binary: stage -> verify hash -> swap with retries -> verify again
try {
    $src = Get-Item $new -ErrorAction Stop
    Log "source: $($src.FullName) size=$($src.Length) time=$($src.LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))"
    $srcHash = Get-Sha256 $new
    Log "source sha256: $srcHash"

    $tmp = "$bin.new"
    Copy-Item $new $tmp -Force -ErrorAction Stop
    $tmpHash = Get-Sha256 $tmp
    if ($tmpHash -ne $srcHash) { throw "staged copy hash mismatch: $tmpHash" }
    Log "staged copy verified: $tmpHash"

    $ok = $false
    for ($i = 1; $i -le 20; $i++) {
        try {
            Move-Item $tmp $bin -Force -ErrorAction Stop
            $ok = $true
            break
        } catch {
            Log "swap attempt $i failed: $($_.Exception.Message)"
            Start-Sleep -Milliseconds 500
        }
    }
    if (-not $ok) { throw 'could not replace installed binary after 20 attempts' }

    $dst = Get-Item $bin
    $dstHash = Get-Sha256 $bin
    Log "binary replaced: size=$($dst.Length) sha256=$dstHash"
    if ($dstHash -ne $srcHash) { throw "installed hash mismatch: $dstHash" }
    Log 'installed binary hash verified OK'
} catch {
    Log "REPLACE FAILED: $($_.Exception.Message)"
}

# 4. start the daemon with the new binary
Log 'daemon start'
& $bin daemon start *>> $log

$deadline = (Get-Date).AddSeconds(30)
$healthy = $false
while ((Get-Date) -lt $deadline) {
    $out = & $bin daemon status 2>&1 | Out-String
    if ($out -match 'healthy: true') { $healthy = $true; break }
    Start-Sleep -Milliseconds 500
}
Log "post-start health check: healthy=$healthy"
Log '=== restart done (verified) ==='

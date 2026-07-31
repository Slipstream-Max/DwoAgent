---
name: dwo-build-deploy
description: Build, install, and restart the dwoagent daemon on Windows (aarch64-pc-windows-msvc). Use when compiling dwoagent from source, deploying a new binary to ~/.dwoagent/bin, restarting the daemon so a change goes live, or troubleshooting release builds that fail on clang/ring or binary replacement that fails because the exe is locked by the running daemon.
---

# Dwo Build & Deploy

Build dwoagent from source, replace the installed binary, and restart the daemon so the change goes live. This machine is `aarch64-pc-windows-msvc`; the `ring` crate requires `clang`, which is only available inside the Visual Studio Build Tools dev shell.

## Build environment

Enter the VS dev shell before any `cargo` command that touches `dwo-agent` (or any crate that depends on `reqwest`/`ring`):

```powershell
Import-Module "C:\Users\11307\Develop\VSBuildTools\Common7\Tools\Microsoft.VisualStudio.DevShell.dll"
Enter-VsDevShell f80da439 -SkipAutomaticLocation -DevCmdArguments "-arch=arm64 -host_arch=arm64"
```

After entering, `clang --version` must print `clang version 20.x`. Without it, release builds fail with `error occurred in cc-rs: failed to find tool "clang"`.

`dwo-tools` alone does not need clang; plain `cargo test -p dwo-tools` works in any shell.

## Build

```powershell
cargo build --release -p dwo-agent   # output: target\release\dwo.exe
```

Incremental release builds take about 3 minutes; cold builds about 6.

## Test

```powershell
cargo test -p dwo-tools          # any shell
cargo test -p dwo-agent          # dev shell (ring needs clang)
```

Cargo writes progress to stderr, so the terminal call may report a non-zero exit code while the build actually succeeded. Judge by `Finished `release` profile` / `test result: ok. N passed` lines, not by exit codes.

## Deploy

Installed binary: `C:\Users\11307\.dwoagent\bin\dwo.exe` (back it up to `dwo.exe.bak` before replacing).

The running daemon locks the exe image, so `Copy-Item` fails with "being used by another process". The correct order is:

1. `dwo daemon stop` (graceful IPC shutdown)
2. Wait until no `dwo.exe` processes remain (up to ~30 s), force-kill leftovers
3. `Copy-Item <repo>\target\release\dwo.exe <bin> -Force`
4. `dwo daemon start`

## Restart (never run stop/start inline from the agent terminal)

Stopping the daemon kills the session that hosts the agent's terminal commands. `dwo daemon stop; dwo daemon start` run inline can die after the stop, leaving the daemon down. Launch the bundled helper detached instead, then end the turn:

```powershell
Start-Process powershell -WindowStyle Hidden -ArgumentList `
  '-NoProfile','-ExecutionPolicy','Bypass','-File', `
  '<skill-directory>\scripts\restart-dwo.ps1'
```

The helper (`scripts/restart-dwo.ps1`) parameterizes repo and profile root, then:

1. sleeps 10 s so the current reply finishes streaming
2. stops the daemon, waits up to 30 s for `dwo.exe` processes to exit, force-kills leftovers
3. copies the freshly built binary into `~/.dwoagent\bin`
4. starts the daemon and waits until healthy
5. appends every step to `~/.dwoagent\runtime\restart.log`

Terminal children are not attached to job objects, so the detached helper survives the daemon shutdown.

## Verify after restart

```powershell
dwo daemon status                                                     # healthy: true
Get-Content "$env:USERPROFILE\.dwoagent\runtime\restart.log"          # binary replaced / daemon started
Get-Item "$env:USERPROFILE\.dwoagent\bin\dwo.exe"                     # LastWriteTime matches the new build
```

Sessions persist across restarts (`runtime\sessions\...`); the user resumes by sending a new message. Never commit the runtime folder or `restart.log`.

## Gotchas

- Release build fails with `clang: program not found` → you are not in the dev shell.
- `Copy-Item ... dwo.exe: being used by another process` → the daemon or a session process still runs; stop it first.
- The old binary may linger after a graceful stop; force-kill remaining `dwo.exe` processes before copying.

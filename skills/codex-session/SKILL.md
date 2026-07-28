---
name: codex-session
description: Run and manage asynchronous Codex CLI sessions from Dwo Agent on Windows. Use when delegating coding, review, investigation, testing, or other repository work to Codex; when starting or continuing a Codex session with an optional cwd, model, or reasoning effort; or when listing, watching, or cancelling delegated Codex work.
---

# Codex Session

Use the bundled PowerShell script as a thin, non-blocking wrapper around Codex CLI. It reuses the current Windows user's Codex configuration and authentication and treats Codex rollout files as the only persistent session state.

## Commands

Run the script with PowerShell 7 and its absolute path inside this skill:

```powershell
pwsh -NoProfile -File "<skill-directory>\scripts\codex-session.ps1" list

pwsh -NoProfile -File "<skill-directory>\scripts\codex-session.ps1" prompt `
  -Message "Inspect the repository and fix the failing tests." `
  -Cwd "C:\path\to\repo" `
  -Model "gpt-5.6-sol" `
  -Reasoning "high"

pwsh -NoProfile -File "<skill-directory>\scripts\codex-session.ps1" prompt `
  -To "<session-id>" `
  -Message "Now implement the fix and run the focused tests."

pwsh -NoProfile -File "<skill-directory>\scripts\codex-session.ps1" watch `
  -SessionId "<session-id>"

pwsh -NoProfile -File "<skill-directory>\scripts\codex-session.ps1" watch `
  -SessionId "<session-id>" `
  -Cursor 123 `
  -Limit 3

pwsh -NoProfile -File "<skill-directory>\scripts\codex-session.ps1" cancel `
  -SessionId "<session-id>"
```

Omit `-Cwd`, `-Model`, and `-Reasoning` unless the task requires an override. A new session defaults to the invoking working directory. A resumed session defaults to its latest recorded working directory.

`list` returns the latest 20 sessions by default; use `-Limit` to change that bound. `watch` returns the latest three content events by default.

## Workflow

1. Call `prompt` once. Treat `status: running` as accepted asynchronous work and retain the returned `sessionId`.
2. Treat `status: busy` as an instruction to wait; do not submit the same prompt again and do not build a prompt queue.
3. Call `watch` without a cursor to inspect the latest three content events. Reuse the returned `cursor` on later calls to read only newer events.
4. Consider `kind: status` with `content: completed` or `content: aborted` terminal for the observed turn. Read the latest assistant message for the result.
5. Call `cancel` only when the user requests cancellation or the delegated work is no longer needed.

Different sessions may run concurrently. One worker owns exactly one Codex turn and never switches sessions. The same session accepts only one active worker.

## State And Configuration

Read-only commands scan `$CODEX_HOME/sessions`, defaulting to `~/.codex/sessions`. Do not create parallel event logs, PID files, or session metadata.

Codex inherits the user's `config.toml`, `auth.json`, provider, approval policy, and sandbox policy. `-Model` and `-Reasoning` override only that invocation. If automatic executable discovery fails, set `DWO_CODEX_PATH` to a directly runnable `codex.exe`.

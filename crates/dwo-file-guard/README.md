# dwo-file-guard

Protects the runtime state of the agent profile while the daemon runs.

```rust
let protection = dwo_file_guard::Protection::install(
    &profile_root,
    &["runtime"],
    &[
        dwo_file_guard::Skip::Directory("workspaces"),
        dwo_file_guard::Skip::Directory("workspace"),
        dwo_file_guard::Skip::File("AGENTS.md"),
        dwo_file_guard::Skip::File("overview.md"),
        dwo_file_guard::Skip::Extension("log"),
        dwo_file_guard::Skip::Extension("tmp"),
    ],
);

let _permit = dwo_file_guard::permit_file(&path);
dwo_agent_service::atomic_file::write(&path, bytes).await?;
```

## Behaviour

| Platform | Capability | Mechanism |
| --- | --- | --- |
| Windows | `DenyWrite` | Every guarded file is held open for read with `FILE_SHARE_READ`. Other handles may only read; write, delete and rename fail with `ERROR_SHARING_VIOLATION` (32) or `ERROR_ACCESS_DENIED` (5). |
| Linux, macOS | `Unsupported` | POSIX has no share modes. `flock` is advisory and `rename` over a read-only file still succeeds, so guarding would be security theatre. Every call is a no-op. |

Writers inside the host go through `permit_file`, which closes the handle for
the duration of one write and re-arms it when the permit drops. Guards are
re-armed lazily, so a file created while the host runs ends up guarded after
its first write. `release_tree` drops guards for a subtree without re-arming
them, which is what deletions use.

Protection is best effort: a file that cannot be opened is counted in
`Report::failed` and never blocks startup. Files created through paths that do
not take a permit (downloaded channel attachments) stay unguarded.

## Policy

The mechanism is generic; the policy lives at the call site. The daemon
protects the whole `runtime` tree and skips:

- directories named `workspaces` and `workspace`: managed session workspaces
  and project working folders that agent tools write to
- `AGENTS.md` and `overview.md`: project and topic rule files that stay
  editable while the host runs
- `*.log`: deploy scripts append to `runtime/restart.log` while the host runs
- `*.tmp`: temporary files a crashed atomic write may have left behind
